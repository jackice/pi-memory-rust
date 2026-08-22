//! Embedding runtime component (M2 refactor): shared global Embedder, per-project vector indexes.
//!
//! With multi-database isolation, the ONNX model (~90MB) is loaded once globally (Arc<Mutex>),
//! and each project (projectScope) owns an independent VectorIndex and id mapping.
//! The RPC main loop is single-threaded and serial, so the Mutex never actually contends.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::embedding::Embedder;
use crate::models::{MemoryEmbeddingRecord, MemoryKind};
use crate::storage::TursoStorage;
use crate::vector_index::VectorIndex;

/// Default model cache directory (PI_MEMORY_MODEL_PATH takes precedence).
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

/// Global shared Embedder handle (the model is loaded exactly once).
#[derive(Clone)]
pub struct SharedEmbedder {
    inner: Arc<Mutex<EmbedderState>>,
    /// Completion signal for background warmup loads (bounded wait before exit avoids racing ONNX global init).
    loading_done: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
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
            loading_done: Arc::new(Mutex::new(None)),
            model_dir: default_model_dir(),
        }
    }

    /// Whether the model directory is complete.
    pub fn model_ready(&self) -> bool {
        crate::model_download::model_dir_complete(&self.model_dir)
    }

    /// Warm up the model in the background; returns immediately when not ready, a load is
    /// already in flight, or the model is Failed (retried once files are repaired).
    pub fn warmup(&self) {
        if !self.model_ready() {
            return;
        }
        let should_start = {
            let mut state = self.inner.lock().expect("mutex poisoned");
            // Both Unloaded and Failed may start: Failed is transient and retried after repair
            if matches!(*state, EmbedderState::Unloaded | EmbedderState::Failed) {
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
        // Signal completion after the state is settled so the process can wait before exit
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let loaded = Embedder::load(&model_dir);
            let mut state = inner.lock().expect("mutex poisoned");
            match loaded {
                Ok(embedder) => {
                    crate::debug_log(&format!("embedding model loaded: {}", model_dir.display()));
                    *state = EmbedderState::Ready(embedder);
                }
                Err(error) => {
                    crate::debug_log(&format!(
                        "embedding model load failed (falling back to FTS-only): {error}\n  model_dir: {}",
                        model_dir.display()
                    ));
                    *state = EmbedderState::Failed;
                }
            }
            let _ = tx.send(());
        });
        *self.loading_done.lock().expect("mutex poisoned") = Some(rx);
    }

    /// Wait (bounded) for the background warmup load to finish; give up on timeout
    /// (only happens when the process exits right after startup).
    pub fn wait_loading(&self, timeout: Duration) {
        let rx = self.loading_done.lock().expect("mutex poisoned").take();
        if let Some(rx) = rx {
            let _ = rx.recv_timeout(timeout);
        }
    }

    /// Ensure the model is loaded; returns false immediately while a background load is in
    /// flight so callers fall back to FTS-only.
    pub fn ensure_loaded(&self) -> bool {
        let should_load = {
            let mut state = self.inner.lock().expect("mutex poisoned");
            match &*state {
                EmbedderState::Ready(_) => return true,
                // Background load in flight: do not start another one, caller degrades to FTS-only
                EmbedderState::Loading => return false,
                // Failed is retryable: reload synchronously once the files are repaired
                EmbedderState::Unloaded | EmbedderState::Failed => {
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
        let mut state = self.inner.lock().expect("mutex poisoned");
        match loaded {
            Ok(embedder) => {
                crate::debug_log(&format!("embedding model loaded: {}", self.model_dir.display()));
                *state = EmbedderState::Ready(embedder);
                true
            }
            Err(error) => {
                crate::debug_log(&format!(
                    "embedding model load failed (falling back to FTS-only): {error}\n  model_dir: {}",
                    self.model_dir.display()
                ));
                *state = EmbedderState::Failed;
                false
            }
        }
    }

    /// Status description (for diagnostics).
    pub fn status(&self) -> String {
        let state = self.inner.lock().expect("mutex poisoned");
        match &*state {
            EmbedderState::Ready(_) => "on".into(),
            EmbedderState::Loading => "loading".into(),
            EmbedderState::Failed if self.model_ready() => "model-error".into(),
            _ => "off (model not ready)".into(),
        }
    }

    /// Encode a batch of texts; returns None while loading or unavailable → caller degrades.
    pub fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        if !self.ensure_loaded() {
            return None;
        }
        let mut state = self.inner.lock().expect("mutex poisoned");
        let EmbedderState::Ready(embedder) = &mut *state else {
            return None;
        };
        embedder.embed(texts).ok()
    }
}

/// Embedding candidate (matches the element returned by the TS buildEmbeddingCandidates).
#[derive(Debug, Clone)]
pub struct EmbeddingCandidate {
    pub memory_kind: MemoryKind,
    pub memory_id: String,
    pub content: String,
    pub score: f64,
}

/// Per-project vector runtime: independent VectorIndex + id mapping (multi-db isolation).
pub struct ProjectVectors {
    index: VectorIndex,
    /// vector_id → (memory_kind, memory_id)
    id_to_memory: HashMap<u64, (MemoryKind, String)>,
    /// (kind, memory_id) → vector_id
    memory_to_id: HashMap<(MemoryKind, String), u64>,
    /// (kind, memory_id) → content (used to inject recall candidates)
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

    /// Rebuild the vector index from the database.
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
        // Take the id base once and assign consecutive ids by the actual index size
        // (does not depend on reading next_id mid-iteration)
        let id_base = self.index.next_id_hint();
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
            let id = id_base + ids.len() as u64;
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

    /// retain path: embed the text, persist it, and add it to the index.
    pub fn save_embedding(
        &mut self,
        storage: &TursoStorage,
        shared: &SharedEmbedder,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let Some(embeddings) = shared.embed(&[text.to_string()]) else {
            return Ok(()); // model not ready: degrade silently
        };
        let embedding = embeddings.into_iter().next().unwrap_or_default();
        self.save_embedding_vector(storage, memory_kind, memory_id, text, embedding)
    }

    /// Persist a precomputed vector and add it to the index (used by benchmarks/bulk fill to avoid per-row inference).
    pub fn save_embedding_vector(
        &mut self,
        storage: &TursoStorage,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
        embedding: Vec<f32>,
    ) -> anyhow::Result<()> {
        // Defensive: a degenerate/empty embedding would corrupt the index (dim 0); skip it
        if embedding.is_empty() {
            return Ok(());
        }
        storage.save_embedding(&MemoryEmbeddingRecord {
            memory_kind,
            memory_id: memory_id.to_string(),
            embedding: embedding.iter().map(|&v| v as f64).collect(),
            model: "all-MiniLM-L6-v2".into(),
        })?;

        let key = (memory_kind, memory_id.to_string());
        self.content_map.insert(key.clone(), text.to_string());
        // Remove an existing vector first (turbovec has no update; delete and rebuild)
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

    /// Bulk-persist vectors and batch-add them to the index (bench/history import; avoids per-row add overhead).
    /// Returns (memory_id → vector_id) only if the caller wants it; this method maintains the mappings internally.
    pub fn save_embeddings_batch(
        &mut self,
        storage: &TursoStorage,
        items: &[(MemoryKind, String, String)], // (kind, memory_id, text)
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<()> {
        debug_assert_eq!(items.len(), embeddings.len());
        // 1) persist vectors
        for (item, embedding) in items.iter().zip(embeddings.iter()) {
            storage.save_embedding(&MemoryEmbeddingRecord {
                memory_kind: item.0,
                memory_id: item.1.clone(),
                embedding: embedding.iter().map(|&v| v as f64).collect(),
                model: "all-MiniLM-L6-v2".into(),
            })?;
        }
        // 2) batch index add
        let ids = self.index.add(embeddings)?;
        for ((item, id), _embedding) in items.iter().zip(ids.iter()).zip(embeddings.iter()) {
            let key = (item.0, item.1.clone());
            self.content_map.insert(key.clone(), item.2.clone());
            self.id_to_memory.insert(*id, key.clone());
            self.memory_to_id.insert(key, *id);
        }
        Ok(())
    }

    /// recall path: FTS candidates → allowlist → re-rank → candidates.
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
                continue; // matches the TS filter(embeddingScore > 0)
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

    /// Number of vectors currently in the index (tests/diagnostics).
    pub fn index_len(&self) -> usize {
        self.index.len()
    }
}
