//! Semantic embedding (M2): ort 2.0 + a manifest-configured sentence encoder.
//!
//! Design notes (following mnemopi experience and the ort sentence-transformers example):
//! - lazy loading: the model loads on first embed request so service startup stays fast;
//! - model files: local path > auto-download (official source/mirror, degrade on failure);
//! - graceful degradation: missing/broken model → returns None, callers fall back to FTS-only;
//! - post-processing: manifest-configured mean/CLS pooling + L2 normalization.

use anyhow::{Context, Result};
use ort::session::Session;
use ort::value::TensorRef;

/// Semantic embedder (lazily initialized; needs &mut because Session::run requires &mut).
pub struct Embedder {
    session: Session,
    tokenizer: tokenizers::Tokenizer,
    dim: usize,
    max_length: usize,
    pooling: String,
    query_prefix: String,
    document_prefix: String,
}

impl Embedder {
    /// Load from the model directory (blocking; call from a dedicated thread).
    pub fn load(model_dir: &std::path::Path) -> Result<Self> {
        let manifest = crate::model_manifest::verify_dir(model_dir)?;
        let model_path = model_dir.join(&manifest.model_file);
        let tokenizer_path = model_dir.join(&manifest.tokenizer_file);
        anyhow::ensure!(
            model_path.exists(),
            "model file missing: {}",
            model_path.display()
        );
        anyhow::ensure!(
            tokenizer_path.exists(),
            "tokenizer missing: {}",
            tokenizer_path.display()
        );

        let session = Session::builder()?
            .with_optimization_level(ort::session::builder::GraphOptimizationLevel::Level1)
            .map_err(|e| anyhow::anyhow!("failed to configure optimization level: {e}"))?
            .with_intra_threads(1)
            .map_err(|e| anyhow::anyhow!("failed to configure threads: {e}"))?
            .commit_from_file(&model_path)
            .with_context(|| format!("failed to load ONNX model: {}", model_path.display()))?;
        let mut tokenizer = tokenizers::Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow::anyhow!("failed to load tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: manifest.max_length,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("failed to configure tokenizer truncation: {e}"))?;
        tokenizer.with_padding(Some(tokenizers::PaddingParams::default()));

        let mut embedder = Self {
            session,
            tokenizer,
            dim: manifest.dimension,
            max_length: manifest.max_length,
            pooling: manifest.pooling,
            query_prefix: manifest.query_prefix,
            document_prefix: manifest.document_prefix,
        };
        let smoke = embedder.embed(&["pi-memory model smoke test".to_string()])?;
        anyhow::ensure!(
            smoke.len() == 1 && smoke[0].len() == manifest.dimension,
            "model smoke embedding returned unexpected dimensions"
        );
        Ok(embedder)
    }

    /// Output dimension.
    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn will_truncate(&self, text: &str) -> bool {
        // Use an untruncated tokenizer clone to determine whether the configured maximum will
        // discard tokens. This check is only called for stored memories and never logs content.
        let mut tokenizer = self.tokenizer.clone();
        if tokenizer.with_truncation(None).is_err() {
            return false;
        }
        let text = format!("{}{text}", self.document_prefix);
        tokenizer
            .encode(text, true)
            .map(|encoding| encoding.len() > self.max_length)
            .unwrap_or(false)
    }

    pub fn embed_query(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_prefixed(texts, true)
    }

    pub fn embed_document(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_prefixed(texts, false)
    }

    fn embed_prefixed(&mut self, texts: &[String], query: bool) -> Result<Vec<Vec<f32>>> {
        let prefix = if query {
            self.query_prefix.clone()
        } else {
            self.document_prefix.clone()
        };
        if prefix.is_empty() {
            return self.embed(texts);
        }
        let prefixed = texts
            .iter()
            .map(|text| format!("{prefix}{text}"))
            .collect::<Vec<_>>();
        self.embed(&prefixed)
    }

    /// Encode a batch of texts using the manifest pooling strategy.
    pub fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Padding and truncation are explicitly configured during load; keep special tokens.
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
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
        let type_ids: Vec<i64> = encodings
            .iter()
            .flat_map(|encoding| encoding.get_type_ids().iter().map(|&id| id as i64))
            .collect();

        let batch = texts.len();
        let mut inputs = Vec::<(String, ort::session::SessionInputValue<'_>)>::new();
        let mut has_input_ids = false;
        let mut has_attention_mask = false;
        for input in self.session.inputs() {
            let name = input.name();
            let value = match name {
                "input_ids" => {
                    has_input_ids = true;
                    TensorRef::from_array_view(([batch, padded_len], &ids[..]))
                        .map_err(|e| anyhow::anyhow!("failed to build input_ids: {e}"))?
                        .into()
                }
                "attention_mask" => {
                    has_attention_mask = true;
                    TensorRef::from_array_view(([batch, padded_len], &mask[..]))
                        .map_err(|e| anyhow::anyhow!("failed to build attention_mask: {e}"))?
                        .into()
                }
                "token_type_ids" => {
                    TensorRef::from_array_view(([batch, padded_len], &type_ids[..]))
                        .map_err(|e| anyhow::anyhow!("failed to build token_type_ids: {e}"))?
                        .into()
                }
                other => anyhow::bail!("unsupported embedding model input: {other}"),
            };
            inputs.push((name.to_string(), value));
        }
        anyhow::ensure!(
            has_input_ids && has_attention_mask && (inputs.len() == 2 || inputs.len() == 3),
            "embedding model must accept input_ids, attention_mask, and optional token_type_ids"
        );

        let outputs = self
            .session
            .run(inputs)
            .with_context(|| "model inference failed")?;
        // outputs[0] = last_hidden_state [batch, seq, hidden]
        let hidden_state = outputs[0]
            .try_extract_array::<f32>()
            .map_err(|e| anyhow::anyhow!("failed to extract output: {e}"))?;
        let hidden_state = hidden_state
            .into_dimensionality::<ndarray::Ix3>()
            .map_err(|e| anyhow::anyhow!("unexpected output dimension: {e}"))?;

        let hidden = hidden_state.shape()[2];
        anyhow::ensure!(
            hidden == self.dim,
            "model output dimension mismatch: manifest {}, model {}",
            self.dim,
            hidden
        );
        let mut result = Vec::with_capacity(batch);
        for b in 0..batch {
            let mut sum = vec![0f32; hidden];
            let attention_mask = encodings[b].get_attention_mask();
            if self.pooling == "cls" {
                anyhow::ensure!(
                    !attention_mask.is_empty(),
                    "empty token sequence from tokenizer"
                );
                for h in 0..hidden {
                    sum[h] = hidden_state[[b, 0, h]];
                }
            } else {
                let mut pooled_tokens = 0usize;
                for (s, &mask_value) in attention_mask.iter().enumerate() {
                    if mask_value == 0 {
                        continue;
                    }
                    for h in 0..hidden {
                        sum[h] += hidden_state[[b, s, h]];
                    }
                    pooled_tokens += 1;
                }
                if pooled_tokens > 0 {
                    for v in &mut sum {
                        *v /= pooled_tokens as f32;
                    }
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
