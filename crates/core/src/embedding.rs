//! Semantic embedding (M2): ort 2.0 + all-MiniLM-L6-v2.
//!
//! Design notes (following mnemopi experience and the ort sentence-transformers example):
//! - lazy loading: the model loads on first embed request so service startup stays fast;
//! - model files: local path > auto-download (official source/mirror, degrade on failure);
//! - graceful degradation: missing/broken model → returns None, callers fall back to FTS-only;
//! - post-processing: mean pooling + L2 normalization (sentence-transformers standard).

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::TensorRef;

/// Model directory layout (PI_MEMORY_MODEL_PATH points at this directory):
///   model.onnx / tokenizer.json / config.json
pub const MODEL_FILES: [&str; 3] = ["model.onnx", "tokenizer.json", "config.json"];

/// Semantic embedder (lazily initialized; needs &mut because Session::run requires &mut).
pub struct Embedder {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
    dim: usize,
}

impl Embedder {
    /// Load from the model directory (blocking; call from a dedicated thread).
    pub fn load(model_dir: &std::path::Path) -> Result<Self> {
        let model_path = model_dir.join("model.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");
        anyhow::ensure!(model_path.exists(), "model file missing: {}", model_path.display());
        anyhow::ensure!(tokenizer_path.exists(), "tokenizer missing: {}", tokenizer_path.display());

        let session = Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level1)
            .map_err(|e| anyhow::anyhow!("failed to configure optimization level: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!("failed to configure threads: {e}"))?
            .commit_from_file(&model_path)
            .with_context(|| format!("failed to load ONNX model: {}", model_path.display()))?;
        let tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;

        // all-MiniLM-L6-v2 always outputs 384 dims
        let dim = 384;

        Ok(Self { session, tokenizer, dim })
    }

    /// Output dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    /// Encode a batch of texts (mean pooling + L2 normalization).
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // encode_batch pads automatically to the longest sequence in the batch
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), false)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;
        let padded_len = encodings[0].len();

        let ids: Vec<i64> = encodings
            .iter()
            .flat_map(|e| e.get_ids().iter().map(|&id| id as i64))
            .collect();
        let mask: Vec<i64> = encodings
            .iter()
            .flat_map(|e| e.get_attention_mask().iter().map(|&m| m as i64))
            .collect();
        // all-MiniLM needs a third input, token_type_ids (always 0 for single-segment input)
        let type_ids: Vec<i64> = vec![0i64; ids.len()];

        let batch = texts.len();
        let ids_tensor = TensorRef::from_array_view(([batch, padded_len], &ids[..]))
            .map_err(|e| anyhow::anyhow!("failed to build input_ids: {e}"))?;
        let mask_tensor = TensorRef::from_array_view(([batch, padded_len], &mask[..]))
            .map_err(|e| anyhow::anyhow!("failed to build attention_mask: {e}"))?;
        let type_tensor = TensorRef::from_array_view(([batch, padded_len], &type_ids[..]))
            .map_err(|e| anyhow::anyhow!("failed to build token_type_ids: {e}"))?;

        let outputs = self
            .session
            .run(ort::inputs![ids_tensor, mask_tensor, type_tensor])
            .with_context(|| "model inference failed")?;
        // outputs[0] = last_hidden_state [batch, seq, hidden]
        let hidden_state = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract output: {e}"))?;
        let hidden_state = hidden_state
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|e| anyhow::anyhow!("unexpected output dimension: {e}"))?;

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
            // L2 normalization
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

