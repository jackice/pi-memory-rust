//! embedding 运行时组件（M2 重构）：Embedder 全局共享，向量索引按项目独立。
//!
//! 多库隔离下：ONNX 模型（~90MB）全局只加载一份（Arc<Mutex>），
//! 每个项目（projectScope）拥有独立的 VectorIndex 与 id 映射，互不串扰。
//! RPC 主循环单线程串行，Mutex 无实际竞争。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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

#[allow(clippy::large_enum_variant)]
enum EmbedderState {
    Unloaded,
    Loading,
    Ready(Embedder),
    Failed,
}

/// 全局共享的 Embedder 句柄（模型只加载一份）。
#[derive(Clone)]
pub struct SharedEmbedder {
    inner: Arc<Mutex<EmbedderState>>,
    model_dir: PathBuf,
}

impl Default for SharedEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedEmbedder {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EmbedderState::Unloaded)),
            model_dir: default_model_dir(),
        }
    }

    /// 模型目录是否完整。
    pub fn model_ready(&self) -> bool {
        crate::model_download::model_dir_complete(&self.model_dir)
    }

    /// 后台预热模型；未就绪或已有加载任务时立即返回。
    pub fn warmup(&self) {
        if !self.model_ready() {
            return;
        }
        let should_start = {
            let mut state = self.inner.lock().expect("锁中毒");
            if matches!(*state, EmbedderState::Unloaded) {
                *state = EmbedderState::Loading;
                true
            } else {
                false
            }
        };
        if !should_start {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let model_dir = self.model_dir.clone();
        std::thread::spawn(move || {
            let loaded = Embedder::load(&model_dir);
            let mut state = inner.lock().expect("锁中毒");
            match loaded {
                Ok(embedder) => {
                    eprintln!("[pi-memory-rust] embedding 模型加载成功: {}", model_dir.display());
                    *state = EmbedderState::Ready(embedder);
                }
                Err(error) => {
                    eprintln!(
                        "[pi-memory-rust] embedding 模型加载失败（降级 FTS-only）: {error}\n  model_dir: {}",
                        model_dir.display()
                    );
                    *state = EmbedderState::Failed;
                }
            }
        });
    }

    /// 确保已加载；后台加载进行中时立即返回 false，让调用方走 FTS-only。
    pub fn ensure_loaded(&self) -> bool {
        let should_load = {
            let mut state = self.inner.lock().expect("锁中毒");
            match &*state {
                EmbedderState::Ready(_) => return true,
                EmbedderState::Loading | EmbedderState::Failed => return false,
                EmbedderState::Unloaded => {
                    if !self.model_ready() {
                        return false;
                    }
                    *state = EmbedderState::Loading;
                    true
                }
            }
        };
        if !should_load {
            return false;
        }
        let loaded = Embedder::load(&self.model_dir);
        let mut state = self.inner.lock().expect("锁中毒");
        match loaded {
            Ok(embedder) => {
                eprintln!("[pi-memory-rust] embedding 模型加载成功: {}", self.model_dir.display());
                *state = EmbedderState::Ready(embedder);
                true
            }
            Err(error) => {
                eprintln!(
                    "[pi-memory-rust] embedding 模型加载失败（降级 FTS-only）: {error}\n  model_dir: {}",
                    self.model_dir.display()
                );
                *state = EmbedderState::Failed;
                false
            }
        }
    }

    /// 状态描述（诊断用）。
    pub fn status(&self) -> String {
        let state = self.inner.lock().expect("锁中毒");
        match &*state {
            EmbedderState::Ready(_) => "on".into(),
            EmbedderState::Loading => "loading".into(),
            EmbedderState::Failed if self.model_ready() => "model-error".into(),
            _ => "off (模型未就绪)".into(),
        }
    }

    /// 编码一批文本；模型加载中或不可用时返回 None → 调用方降级。
    pub fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        if !self.ensure_loaded() {
            return None;
        }
        let mut state = self.inner.lock().expect("锁中毒");
        let EmbedderState::Ready(embedder) = &mut *state else {
            return None;
        };
        embedder.embed(texts).ok()
    }
}

/// embedding 候选（对应 TS 版 buildEmbeddingCandidates 的返回元素）。
#[derive(Debug, Clone)]
pub struct EmbeddingCandidate {
    pub memory_kind: MemoryKind,
    pub memory_id: String,
    pub content: String,
    pub score: f64,
}

/// 项目级向量运行时：独立的 VectorIndex + id 映射（多库隔离）。
pub struct ProjectVectors {
    index: VectorIndex,
    /// vector_id → (memory_kind, memory_id)
    id_to_memory: HashMap<u64, (MemoryKind, String)>,
    /// (kind, memory_id) → vector_id
    memory_to_id: HashMap<(MemoryKind, String), u64>,
    /// (kind, memory_id) → content（召回注入用）
    content_map: HashMap<(MemoryKind, String), String>,
}

impl ProjectVectors {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            index: VectorIndex::new(crate::vector_index::DEFAULT_DIM, None)?,
            id_to_memory: HashMap::new(),
            memory_to_id: HashMap::new(),
            content_map: HashMap::new(),
        })
    }

    /// 从数据库恢复向量索引。
    pub fn hydrate(&mut self, storage: &TursoStorage) -> anyhow::Result<()> {
        let working = storage
            .list_all_working()?
            .into_iter()
            .map(|row| (row.id, row.content))
            .collect::<HashMap<_, _>>();
        let episodic = storage
            .list_all_episodic()?
            .into_iter()
            .map(|row| (row.id, row.content))
            .collect::<HashMap<_, _>>();
        let mut vectors: Vec<Vec<f32>> = Vec::new();
        let mut ids: Vec<u64> = Vec::new();
        let mut items: Vec<(MemoryKind, String, String)> = Vec::new();
        for embedding in storage.list_all_embeddings()? {
            let content = match embedding.memory_kind {
                MemoryKind::Working => working.get(&embedding.memory_id).cloned(),
                MemoryKind::Episodic => episodic.get(&embedding.memory_id).cloned(),
                _ => None,
            };
            let Some(content) = content else {
                continue;
            };
            if embedding.embedding.len() != crate::vector_index::DEFAULT_DIM {
                continue;
            }
            let vector = embedding.embedding.iter().map(|&v| v as f32).collect::<Vec<_>>();
            let id = self.index.next_id_hint() + items.len() as u64;
            vectors.push(vector);
            ids.push(id);
            items.push((embedding.memory_kind, embedding.memory_id, content));
        }
        if vectors.is_empty() {
            return Ok(());
        }
        self.index.add_with_ids(&vectors, &ids)?;
        for ((kind, memory_id, content), id) in items.into_iter().zip(ids) {
            let key = (kind, memory_id.clone());
            self.content_map.insert(key.clone(), content);
            self.id_to_memory.insert(id, key.clone());
            self.memory_to_id.insert(key, id);
        }
        Ok(())
    }

    /// retain 路径：embed 文本并入库 + 入索引。
    pub fn save_embedding(
        &mut self,
        storage: &TursoStorage,
        shared: &SharedEmbedder,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let Some(embeddings) = shared.embed(&[text.to_string()]) else {
            return Ok(()); // 模型未就绪：降级
        };
        let embedding = embeddings.into_iter().next().unwrap_or_default();
        self.save_embedding_vector(storage, memory_kind, memory_id, text, embedding)
    }

    /// 预计算向量入库 + 入索引（基准/批量填充用，避免逐条推理）。
    pub fn save_embedding_vector(
        &mut self,
        storage: &TursoStorage,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
        embedding: Vec<f32>,
    ) -> anyhow::Result<()> {
        storage.save_embedding(&MemoryEmbeddingRecord {
            memory_kind,
            memory_id: memory_id.to_string(),
            embedding: embedding.iter().map(|&v| v as f64).collect(),
            model: "all-MiniLM-L6-v2".into(),
        })?;

        let key = (memory_kind, memory_id.to_string());
        self.content_map.insert(key.clone(), text.to_string());
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

    /// 批量向量入库 + 批量索引 add（bench/历史导入用，避免逐条 add 开销）。
    /// 返回 (memory_id → vector_id) 由调用方决定是否记录；本方法内部维护映射。
    pub fn save_embeddings_batch(
        &mut self,
        storage: &TursoStorage,
        items: &[(MemoryKind, String, String)], // (kind, memory_id, text)
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<()> {
        debug_assert_eq!(items.len(), embeddings.len());
        // 1) 向量落库
        for (item, embedding) in items.iter().zip(embeddings.iter()) {
            storage.save_embedding(&MemoryEmbeddingRecord {
                memory_kind: item.0,
                memory_id: item.1.clone(),
                embedding: embedding.iter().map(|&v| v as f64).collect(),
                model: "all-MiniLM-L6-v2".into(),
            })?;
        }
        // 2) 批量索引 add
        let ids = self.index.add(embeddings)?;
        for ((item, id), _embedding) in items.iter().zip(ids.iter()).zip(embeddings.iter()) {
            let key = (item.0, item.1.clone());
            self.content_map.insert(key.clone(), item.2.clone());
            self.id_to_memory.insert(*id, key.clone());
            self.memory_to_id.insert(key, *id);
        }
        Ok(())
    }

    /// recall 路径：FTS 候选 → allowlist → 精排 → 候选。
    pub fn build_embedding_candidates(
        &self,
        shared: &SharedEmbedder,
        prompt: &str,
        fts_candidate_memories: &[(MemoryKind, String)],
    ) -> anyhow::Result<Vec<EmbeddingCandidate>> {
        let Some(query) = shared.embed(&[prompt.to_string()]).and_then(|v| v.into_iter().next()) else {
            return Ok(Vec::new());
        };
        if query.is_empty() {
            return Ok(Vec::new());
        }

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
                    content: self.content_map.get(&(*kind, memory_id.clone())).cloned().unwrap_or_default(),
                    score: hit.score as f64,
                });
            }
        }
        Ok(candidates)
    }
}
