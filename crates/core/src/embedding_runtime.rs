//! embedding 运行时组件（M2）：embedder + turbovec 索引 + 记忆 id 映射。
//!
//! 职责：
//! - 延迟加载 ONNX 模型（失败 → 降级 FTS-only，不阻塞主流程）；
//! - retain 时：embed 文本 → 存 DB（memory_embeddings）→ 入向量索引；
//! - recall 时：embed prompt → FTS 候选 id 集合作 allowlist → turbovec 精排。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::embedding::Embedder;
use crate::models::{MemoryEmbeddingRecord, MemoryKind};
use crate::storage::TursoStorage;
use crate::vector_index::VectorIndex;

/// 模型默认缓存目录（PI_MEMORY_MODEL_PATH 优先）。
pub fn default_model_dir() -> PathBuf {
    if let Ok(path) = std::env::var("PI_MEMORY_MODEL_PATH") {
        return PathBuf::from(path);
    }
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".cache").join("pi-memory-rust").join("model"))
        .unwrap_or_else(|_| std::env::temp_dir().join("pi-memory-rust").join("model"))
}

/// embedding 运行时（embedder 可缺失 → 降级）。
pub struct EmbeddingRuntime {
    embedder: Option<Embedder>,
    index: VectorIndex,
    /// vector_id → (memory_kind, memory_id)
    id_to_memory: HashMap<u64, (MemoryKind, String)>,
    /// (kind, memory_id) → vector_id
    memory_to_id: HashMap<(MemoryKind, String), u64>,
    model_dir: PathBuf,
}

impl EmbeddingRuntime {
    /// 创建运行时并尝试加载模型（失败仅记录，不报错）。
    pub fn new() -> Self {
        let model_dir = default_model_dir();
        let embedder = match crate::model_download::model_dir_complete(&model_dir) {
            true => match Embedder::load(&model_dir) {
                Ok(e) => Some(e),
                Err(e) => {
                    eprintln!("[pi-memory-rust] embedding 模型加载失败（降级 FTS-only）: {e}");
                    None
                }
            },
            false => {
                eprintln!(
                    "[pi-memory-rust] embedding 模型未就绪（{}），首次 embed 时尝试下载",
                    model_dir.display()
                );
                None
            }
        };
        Self {
            embedder,
            index: VectorIndex::new(crate::vector_index::DEFAULT_DIM, None).expect("创建向量索引失败"),
            id_to_memory: HashMap::new(),
            memory_to_id: HashMap::new(),
            model_dir,
        }
    }

    /// 是否可用（模型已加载）。
    pub fn enabled(&self) -> bool {
        self.embedder.is_some()
    }

    /// 状态描述（诊断用）。
    pub fn status(&self) -> String {
        if self.enabled() {
            "on".into()
        } else if self.model_dir.join("model.onnx").exists() {
            "model-error".into()
        } else {
            "off (模型未就绪)".into()
        }
    }

    /// 确保模型就绪：仅当模型文件已完整时才尝试加载（不触发下载）。
    /// 下载由启动后台线程或 /memory prepare-model 命令负责。
    pub fn ensure_ready(&mut self) -> bool {
        if self.enabled() {
            return true;
        }
        if !crate::model_download::model_dir_complete(&self.model_dir) {
            return false; // 未就绪，不阻塞
        }
        match Embedder::load(&self.model_dir) {
            Ok(e) => {
                self.embedder = Some(e);
                true
            }
            Err(e) => {
                eprintln!("[pi-memory-rust] embedding 模型加载失败（降级 FTS-only）: {e}");
                false
            }
        }
    }

    /// 模型目录是否完整（诊断用）。
    pub fn model_ready(&self) -> bool {
        crate::model_download::model_dir_complete(&self.model_dir)
    }

    /// retain 路径：embed 文本并入库 + 入索引。
    pub fn save_embedding(
        &mut self,
        storage: &TursoStorage,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        if !self.ensure_ready() {
            return Ok(());
        }
        let embedder = self.embedder.as_mut().expect("ensure_ready 已保证");
        let embeddings = embedder.embed(&[text.to_string()])?;
        let embedding = embeddings.into_iter().next().unwrap_or_default();
        storage.save_embedding(&MemoryEmbeddingRecord {
            memory_kind,
            memory_id: memory_id.to_string(),
            embedding: embedding.iter().map(|&v| v as f64).collect(),
            model: "all-MiniLM-L6-v2".into(),
        })?;

        let key = (memory_kind, memory_id.to_string());
        // 已有向量则先移除（turbovec 无 update，删除重建）
        if let Some(old_id) = self.memory_to_id.remove(&key) {
            let _ = self.index.remove(old_id);
            self.id_to_memory.remove(&old_id);
        }
        let ids = self.index.add(&[embedding])?;
        let vector_id = ids[0];
        self.id_to_memory.insert(vector_id, key.clone());
        self.memory_to_id.insert(key, vector_id);
        Ok(())
    }

    /// recall 路径：FTS 候选 memory_id 集合 → allowlist 精排 → 候选。
    pub fn build_embedding_candidates(
        &mut self,
        prompt: &str,
        fts_candidate_memories: &[(MemoryKind, String)],
    ) -> anyhow::Result<Vec<EmbeddingCandidate>> {
        if !self.ensure_ready() {
            return Ok(Vec::new());
        }
        let embedder = self.embedder.as_mut().expect("ensure_ready 已保证");
        let query = embedder.embed(&[prompt.to_string()])?.into_iter().next().unwrap_or_default();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        // allowlist：只在 FTS 候选且有向量的记忆里精排
        let allowlist: Vec<u64> = fts_candidate_memories
            .iter()
            .filter_map(|(kind, id)| self.memory_to_id.get(&(*kind, id.clone())).copied())
            .collect();
        let hits = if allowlist.is_empty() {
            self.index.search(&query, fts_candidate_memories.len().max(1))?
        } else {
            self.index.search_with_allowlist(&query, fts_candidate_memories.len(), Some(&allowlist))?
        };

        let mut candidates = Vec::new();
        for hit in hits {
            if hit.score <= 0.0 {
                continue; // 与 TS 版 filter(embeddingScore > 0) 一致
            }
            if let Some((kind, memory_id)) = self.id_to_memory.get(&hit.id) {
                candidates.push(EmbeddingCandidate {
                    memory_kind: *kind,
                    memory_id: memory_id.clone(),
                    score: hit.score as f64,
                });
            }
        }
        Ok(candidates)
    }
}

/// embedding 候选（对应 TS 版 buildEmbeddingCandidates 的返回元素）。
#[derive(Debug, Clone)]
pub struct EmbeddingCandidate {
    pub memory_kind: MemoryKind,
    pub memory_id: String,
    pub score: f64,
}
