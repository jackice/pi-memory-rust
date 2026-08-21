//! 语义 embedding（M2）：ort 2.0 + all-MiniLM-L6-v2。
//!
//! 设计要点（参照 mnemopi 经验与 ort 官方 sentence-transformers 示例）：
//! - 延迟加载：首次需要 embed 时才加载模型，不拖慢服务启动；
//! - 模型文件：本地路径 > 自动下载（官方源/镜像，失败降级）；
//! - 失败降级：模型缺失/加载失败 → 返回 None，调用方回退 FTS-only；
//! - 后处理：mean pooling + L2 归一化（sentence-transformers 标准）。

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::TensorRef;

/// 模型目录布局（PI_MEMORY_MODEL_PATH 指向该目录）：
///   model.onnx / tokenizer.json / config.json
pub const MODEL_FILES: [&str; 3] = ["model.onnx", "tokenizer.json", "config.json"];

/// 语义 embedding 器（延迟初始化，使用需 &mut 因为 Session::run 需要 &mut）。
pub struct Embedder {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
    dim: usize,
}

impl Embedder {
    /// 从模型目录加载（阻塞操作，调用方应在独立线程执行）。
    pub fn load(model_dir: &std::path::Path) -> Result<Self> {
        let model_path = model_dir.join("model.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");
        anyhow::ensure!(model_path.exists(), "模型文件缺失: {}", model_path.display());
        anyhow::ensure!(tokenizer_path.exists(), "分词器缺失: {}", tokenizer_path.display());

        let session = Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level1)
            .map_err(|e| anyhow::anyhow!("配置优化级别失败: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!("配置线程失败: {e}"))?
            .commit_from_file(&model_path)
            .with_context(|| format!("加载 ONNX 模型失败: {}", model_path.display()))?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("加载分词器失败: {e}"))?;

        // all-MiniLM-L6-v2 固定输出 384 维
        let dim = 384;

        Ok(Self { session, tokenizer, dim })
    }

    /// 输出维度。
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 对一批文本编码（mean pooling + L2 归一化）。
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // encode_batch 自动 padding 到 batch 内最长
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), false)
            .map_err(|e| anyhow::anyhow!("分词失败: {e}"))?;
        let padded_len = encodings[0].len();

        let ids: Vec<i64> = encodings
            .iter()
            .flat_map(|e| e.get_ids().iter().map(|&id| id as i64))
            .collect();
        let mask: Vec<i64> = encodings
            .iter()
            .flat_map(|e| e.get_attention_mask().iter().map(|&m| m as i64))
            .collect();

        let batch = texts.len();
        let ids_tensor = TensorRef::from_array_view(([batch, padded_len], &ids[..]))
            .map_err(|e| anyhow::anyhow!("构造 input_ids 失败: {e}"))?;
        let mask_tensor = TensorRef::from_array_view(([batch, padded_len], &mask[..]))
            .map_err(|e| anyhow::anyhow!("构造 attention_mask 失败: {e}"))?;

        let outputs = self
            .session
            .run(ort::inputs![ids_tensor, mask_tensor])
            .with_context(|| "模型推理失败")?;
        // outputs[0] = last_hidden_state [batch, seq, hidden]
        let hidden_state = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| anyhow::anyhow!("提取输出失败: {e}"))?;
        let hidden_state = hidden_state
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|e| anyhow::anyhow!("输出维度异常: {e}"))?;

        let hidden = hidden_state.shape()[2];
        let mut result = Vec::with_capacity(batch);
        for b in 0..batch {
            let seq_len = encodings[b].get_attention_mask().iter().filter(|&&m| m == 1).count();
            let mut sum = vec![0f32; hidden];
            if seq_len > 0 {
                for s in 0..seq_len {
                    for h in 0..hidden {
                        sum[h] += hidden_state[[b, s, h]];
                    }
                }
                for v in &mut sum {
                    *v /= seq_len as f32;
                }
            }
            // L2 归一化
            let norm: f32 = sum.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in &mut sum {
                    *v /= norm;
                }
            }
            result.push(sum);
        }
        Ok(result)
    }
}
