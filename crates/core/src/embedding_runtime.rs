//! Embedding runtime component (M2 refactor): shared global Embedder, per-project vector indexes.
//!
//! With multi-database isolation, the ONNX model (~90MB) is loaded once globally (Arc<Mutex>),
//! and each project (projectScope) owns an independent VectorIndex and id mapping.
//! Project runtimes serialize per scope while independent scopes can serve requests concurrently.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::embedding::Embedder;
use crate::models::{MemoryEmbeddingRecord, MemoryKind};
use crate::storage::TursoStorage;
use crate::vector_index::VectorIndex;

pub const EMBEDDING_MODEL_ID: &str = "all-MiniLM-L6-v2";

pub fn active_model_id() -> String {
    std::env::var("PI_MEMORY_EMBEDDING_MODEL")
        .ok()
        .filter(|id| !id.trim().is_empty())
        .unwrap_or_else(|| EMBEDDING_MODEL_ID.to_string())
}

/// Embedding seam used by runtime code and deterministic test providers.
pub trait EmbeddingProvider: Send + Sync {
    fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>>;
    fn embed_query(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        self.embed(texts)
    }
    fn embed_document(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        self.embed(texts)
    }
    fn model_id(&self) -> &str;
    fn status(&self) -> String;
    fn is_ready(&self) -> bool;
    fn will_truncate(&self, _text: &str) -> Option<bool> {
        None
    }
}

/// Default model cache directory (PI_MEMORY_MODEL_PATH takes precedence).
pub fn default_model_dir() -> PathBuf {
    if let Ok(path) = std::env::var("PI_MEMORY_MODEL_PATH") {
        return PathBuf::from(path);
    }
    std::env::var("HOME")
        .map(|home| {
            PathBuf::from(home)
                .join(".cache")
                .join("pi-memory-rust")
                .join("model")
        })
        .unwrap_or_else(|_| std::env::temp_dir().join("pi-memory-rust").join("model"))
}

#[allow(clippy::large_enum_variant)]
enum EmbedderState {
    Unloaded,
    Loading,
    Ready(Embedder),
    Failed {
        attempts: u32,
        retry_at: Instant,
        message: String,
    },
}

/// Global shared Embedder handle (the model is loaded exactly once).
#[derive(Clone)]
pub struct SharedEmbedder {
    inner: Arc<Mutex<EmbedderState>>,
    /// Completion signal for background warmup loads (bounded wait before exit avoids racing ONNX global init).
    loading_done: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    model_dir: PathBuf,
    model_id: String,
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
            model_id: active_model_id(),
        }
    }

    /// Whether the model directory is complete.
    pub fn model_ready(&self) -> bool {
        crate::model_download::model_dir_complete(&self.model_dir)
    }

    /// Warm up the model in the background; retries failures with capped exponential backoff.
    pub fn warmup(&self) {
        if !self.model_ready() {
            return;
        }
        let attempts = {
            let mut state = lock_unpoisoned(&self.inner, "embedder state");
            let attempts = match &*state {
                EmbedderState::Unloaded => 0,
                EmbedderState::Failed {
                    attempts, retry_at, ..
                } if Instant::now() >= *retry_at => *attempts,
                _ => return,
            };
            *state = EmbedderState::Loading;
            attempts
        };
        let inner = Arc::clone(&self.inner);
        let model_dir = self.model_dir.clone();
        // Signal completion after the state is settled so the process can wait before exit
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let loaded = Embedder::load(&model_dir);
            let mut state = lock_unpoisoned(&inner, "embedder state");
            match loaded {
                Ok(embedder) => {
                    crate::debug_log(&format!("embedding model loaded: {}", model_dir.display()));
                    *state = EmbedderState::Ready(embedder);
                }
                Err(error) => {
                    let attempts = attempts.saturating_add(1);
                    let backoff_secs = 1u64
                        .checked_shl(attempts.saturating_sub(1).min(8))
                        .unwrap_or(256)
                        .min(300);
                    let message = error.to_string();
                    crate::debug_log(&format!(
                        "embedding model load failed (retry in {backoff_secs}s, falling back to FTS-only): {error}\n  model_dir: {}",
                        model_dir.display()
                    ));
                    *state = EmbedderState::Failed {
                        attempts,
                        retry_at: Instant::now() + Duration::from_secs(backoff_secs),
                        message,
                    };
                }
            }
            let _ = tx.send(());
        });
        *lock_unpoisoned(&self.loading_done, "model loading signal") = Some(rx);
    }

    /// Wait (bounded) for the background warmup load to finish; give up on timeout
    /// (only happens when the process exits right after startup).
    pub fn wait_loading(&self, timeout: Duration) {
        let rx = lock_unpoisoned(&self.loading_done, "model loading signal").take();
        if let Some(rx) = rx {
            let _ = rx.recv_timeout(timeout);
        }
    }

    /// Check whether the model is loaded. Request paths must never load ONNX synchronously;
    /// startup warmup and explicit preparation own all loading work.
    pub fn ensure_loaded(&self) -> bool {
        matches!(
            *lock_unpoisoned(&self.inner, "embedder state"),
            EmbedderState::Ready(_)
        )
    }

    /// Status description (for diagnostics).
    pub fn status(&self) -> String {
        let state = lock_unpoisoned(&self.inner, "embedder state");
        match &*state {
            EmbedderState::Ready(_) => "on".into(),
            EmbedderState::Loading => "loading".into(),
            EmbedderState::Failed { message, .. } => format!("model-error: {message}"),
            _ => "off (model not ready)".into(),
        }
    }

    /// Encode a batch of texts; returns None while loading or unavailable → caller degrades.
    pub fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        self.embed_with_mode(texts, None)
    }

    fn embed_with_mode(&self, texts: &[String], query: Option<bool>) -> Option<Vec<Vec<f32>>> {
        if !self.ensure_loaded() {
            return None;
        }
        let mut state = lock_unpoisoned(&self.inner, "embedder state");
        let EmbedderState::Ready(embedder) = &mut *state else {
            return None;
        };
        match query {
            Some(true) => embedder.embed_query(texts).ok(),
            Some(false) => embedder.embed_document(texts).ok(),
            None => embedder.embed(texts).ok(),
        }
    }
}

impl EmbeddingProvider for SharedEmbedder {
    fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        SharedEmbedder::embed(self, texts)
    }

    fn embed_query(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        self.embed_with_mode(texts, Some(true))
    }

    fn embed_document(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        self.embed_with_mode(texts, Some(false))
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn status(&self) -> String {
        SharedEmbedder::status(self)
    }

    fn is_ready(&self) -> bool {
        self.ensure_loaded()
    }

    fn will_truncate(&self, text: &str) -> Option<bool> {
        let state = lock_unpoisoned(&self.inner, "embedder state");
        match &*state {
            EmbedderState::Ready(embedder) => Some(embedder.will_truncate(text)),
            _ => None,
        }
    }
}

/// Deterministic vector provider for non-release builds and integration-test fixtures.
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, Default)]
pub struct FakeEmbedder;

#[cfg(debug_assertions)]
impl EmbeddingProvider for FakeEmbedder {
    fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>> {
        Some(
            texts
                .iter()
                .map(|text| {
                    let dim = crate::model_manifest::configured_dimension()
                        .unwrap_or(crate::vector_index::DEFAULT_DIM);
                    let mut vector = vec![0.0f32; dim];
                    for token in crate::text::terms(text) {
                        let hash = token.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
                            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
                        });
                        let slot = (hash as usize) % vector.len();
                        vector[slot] += if hash & 1 == 0 { 1.0 } else { -1.0 };
                    }
                    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        for value in &mut vector {
                            *value /= norm;
                        }
                    }
                    vector
                })
                .collect(),
        )
    }

    fn model_id(&self) -> &str {
        "fake-deterministic-384"
    }

    fn status(&self) -> String {
        "on (fake deterministic provider)".into()
    }

    fn is_ready(&self) -> bool {
        true
    }
}

/// Embedding candidate (matches the element returned by the TS buildEmbeddingCandidates).
#[derive(Debug, Clone)]
pub struct EmbeddingCandidate {
    pub memory_kind: MemoryKind,
    pub memory_id: String,
    pub content: String,
    pub project_scope: String,
    pub consolidated: bool,
    pub superseded: bool,
    pub valid_until: Option<String>,
    pub score: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingOutcome {
    Saved,
    SkippedUnavailable,
    SkippedEmpty,
}

/// Per-project vector runtime: independent VectorIndex + id mapping (multi-db isolation).
pub struct ProjectVectors {
    index: VectorIndex,
    dim: usize,
    /// vector_id → (memory_kind, memory_id)
    id_to_memory: HashMap<u64, (MemoryKind, String)>,
    /// (kind, memory_id) → vector_id
    memory_to_id: HashMap<(MemoryKind, String), u64>,
    warned_truncations: HashSet<(MemoryKind, String)>,
}

impl ProjectVectors {
    pub fn new() -> anyhow::Result<Self> {
        let dim = crate::model_manifest::configured_dimension()?;
        Ok(Self {
            index: VectorIndex::new(dim, None)?,
            dim,
            id_to_memory: HashMap::new(),
            memory_to_id: HashMap::new(),
            warned_truncations: HashSet::new(),
        })
    }

    /// Rebuild the vector index from the database.
    pub fn hydrate(&mut self, storage: &TursoStorage) -> anyhow::Result<()> {
        self.hydrate_with_model(storage, &active_model_id())
    }

    pub fn hydrate_with_model(
        &mut self,
        storage: &TursoStorage,
        model_id: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            model_id == active_model_id(),
            "cannot hydrate model {model_id} with the active model's {}-dimension index",
            self.dim
        );
        // Build off to the side, then replace the live index atomically. Repeated hydration
        // must never append duplicate vectors or leave mappings from deleted DB rows behind.
        let next_index = VectorIndex::new(self.dim, None)?;
        let mut next_id_to_memory = HashMap::new();
        let mut next_memory_to_id = HashMap::new();
        let mut after_kind = String::new();
        let mut after_id = String::new();
        let mut next_vector_id = 0_u64;
        loop {
            let page =
                storage.list_embedding_hydration_page(model_id, &after_kind, &after_id, 512)?;
            if page.is_empty() {
                break;
            }
            let mut vectors = Vec::with_capacity(page.len());
            let mut ids = Vec::with_capacity(page.len());
            let mut items = Vec::with_capacity(page.len());
            for embedding in page {
                after_kind = embedding.memory_kind.as_str().to_string();
                after_id.clone_from(&embedding.memory_id);
                if embedding.embedding.len() != self.dim {
                    storage.delete_embedding_for_model(
                        embedding.memory_kind,
                        &embedding.memory_id,
                        model_id,
                    )?;
                    continue;
                }
                if !embedding
                    .embedding
                    .iter()
                    .all(|value| value.is_finite() && (*value as f32).is_finite())
                {
                    storage.delete_embedding_for_model(
                        embedding.memory_kind,
                        &embedding.memory_id,
                        model_id,
                    )?;
                    crate::debug_log(&format!(
                        "removed invalid current-model embedding: {}/{}",
                        embedding.memory_kind.as_str(),
                        embedding.memory_id
                    ));
                    continue;
                }
                let vector = embedding
                    .embedding
                    .iter()
                    .map(|&v| v as f32)
                    .collect::<Vec<_>>();
                let id = next_vector_id;
                next_vector_id = next_vector_id
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("vector hydration id overflow"))?;
                vectors.push(vector);
                ids.push(id);
                items.push((embedding.memory_kind, embedding.memory_id));
            }
            next_index.add_with_ids(&vectors, &ids)?;
            for ((kind, memory_id), id) in items.into_iter().zip(ids) {
                let key = (kind, memory_id);
                next_id_to_memory.insert(id, key.clone());
                next_memory_to_id.insert(key, id);
            }
        }
        self.index = next_index;
        self.id_to_memory = next_id_to_memory;
        self.memory_to_id = next_memory_to_id;
        Ok(())
    }

    /// retain path: embed the text, persist it, and add it to the index.
    pub fn save_embedding(
        &mut self,
        storage: &TursoStorage,
        shared: &dyn EmbeddingProvider,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
    ) -> anyhow::Result<EmbeddingOutcome> {
        self.warn_if_truncated(shared, memory_kind, memory_id, text);
        let Some(embeddings) = shared.embed_document(&[text.to_string()]) else {
            return Ok(EmbeddingOutcome::SkippedUnavailable);
        };
        let embedding = embeddings.into_iter().next().unwrap_or_default();
        self.save_embedding_vector_with_model(
            storage,
            memory_kind,
            memory_id,
            text,
            embedding,
            shared.model_id(),
        )
    }

    /// Persist a precomputed vector and add it to the index (used by benchmarks/bulk fill to avoid per-row inference).
    pub fn save_embedding_vector(
        &mut self,
        storage: &TursoStorage,
        memory_kind: MemoryKind,
        memory_id: &str,
        _text: &str,
        embedding: Vec<f32>,
    ) -> anyhow::Result<EmbeddingOutcome> {
        self.save_embedding_vector_with_model(
            storage,
            memory_kind,
            memory_id,
            _text,
            embedding,
            &active_model_id(),
        )
    }

    pub fn save_embedding_vector_with_model(
        &mut self,
        storage: &TursoStorage,
        memory_kind: MemoryKind,
        memory_id: &str,
        _text: &str,
        embedding: Vec<f32>,
        model_id: &str,
    ) -> anyhow::Result<EmbeddingOutcome> {
        // Defensive: a degenerate/empty embedding would corrupt the index (dim 0); skip it
        if embedding.is_empty() {
            return Ok(EmbeddingOutcome::SkippedEmpty);
        }
        anyhow::ensure!(
            embedding.len() == self.dim,
            "embedding dimension mismatch: expected {}, got {}",
            self.dim,
            embedding.len()
        );
        anyhow::ensure!(
            embedding.iter().all(|value| value.is_finite()),
            "embedding contains a non-finite value"
        );
        storage.save_embedding(&MemoryEmbeddingRecord {
            memory_kind,
            memory_id: memory_id.to_string(),
            embedding: embedding.iter().map(|&v| v as f64).collect(),
            model: model_id.into(),
        })?;

        let key = (memory_kind, memory_id.to_string());
        // Add first: if the index rejects the new vector, the prior mapping remains usable.
        let ids = match self.index.add(&[embedding]) {
            Ok(ids) => ids,
            Err(error) => {
                storage.mark_vector_rehydrate_failed(&error.to_string())?;
                return Err(error);
            }
        };
        let vector_id = ids[0];
        let old_id = self.memory_to_id.insert(key.clone(), vector_id);
        self.id_to_memory.insert(vector_id, key.clone());
        if let Some(old_id) = old_id {
            if let Err(error) = self.index.remove(old_id) {
                storage.mark_vector_rehydrate_failed(&error.to_string())?;
                crate::debug_log(&format!(
                    "old vector removal failed; project rehydrate required: {error}"
                ));
            }
            self.id_to_memory.remove(&old_id);
        }
        Ok(EmbeddingOutcome::Saved)
    }

    pub fn warn_if_truncated(
        &mut self,
        shared: &dyn EmbeddingProvider,
        memory_kind: MemoryKind,
        memory_id: &str,
        text: &str,
    ) {
        if shared.will_truncate(text) != Some(true) {
            return;
        }
        if self
            .warned_truncations
            .insert((memory_kind, memory_id.to_string()))
        {
            crate::debug_log(&format!(
                "embedding input exceeds tokenizer limit; truncated for {}/{}",
                memory_kind.as_str(),
                memory_id
            ));
        }
    }

    /// Bulk-persist vectors and batch-add them to the index (bench/history import; avoids per-row add overhead).
    /// Returns (memory_id → vector_id) only if the caller wants it; this method maintains the mappings internally.
    pub fn save_embeddings_batch(
        &mut self,
        storage: &TursoStorage,
        items: &[(MemoryKind, String, String)], // (kind, memory_id, text)
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<()> {
        self.save_embeddings_batch_with_model(storage, items, embeddings, &active_model_id())
    }

    pub fn save_embeddings_batch_with_model(
        &mut self,
        storage: &TursoStorage,
        items: &[(MemoryKind, String, String)],
        embeddings: &[Vec<f32>],
        model_id: &str,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            items.len() == embeddings.len(),
            "embedding batch item/vector length mismatch: {} items, {} vectors",
            items.len(),
            embeddings.len()
        );
        if items.is_empty() {
            return Ok(());
        }
        for (index, embedding) in embeddings.iter().enumerate() {
            anyhow::ensure!(
                embedding.len() == self.dim,
                "embedding at batch position {index} has dimension {}, expected {}",
                embedding.len(),
                self.dim
            );
            anyhow::ensure!(
                embedding.iter().all(|value| value.is_finite()),
                "embedding at batch position {index} contains a non-finite value"
            );
        }
        // Persist the whole batch atomically before mutating the in-memory index.
        storage.begin_tx()?;
        let write_result = (|| -> anyhow::Result<()> {
            for (item, embedding) in items.iter().zip(embeddings.iter()) {
                storage.save_embedding(&MemoryEmbeddingRecord {
                    memory_kind: item.0,
                    memory_id: item.1.clone(),
                    embedding: embedding.iter().map(|&v| v as f64).collect(),
                    model: model_id.into(),
                })?;
            }
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = storage.rollback_tx();
            return Err(error);
        }
        if let Err(error) = storage.commit_tx() {
            let _ = storage.rollback_tx();
            return Err(error);
        }
        // 2) Batch-add before replacing mappings, so failure leaves the old index entries usable.
        let ids = match self.index.add(embeddings) {
            Ok(ids) => ids,
            Err(error) => {
                storage.mark_vector_rehydrate_failed(&error.to_string())?;
                return Err(error);
            }
        };
        for ((item, id), _embedding) in items.iter().zip(ids.iter()).zip(embeddings.iter()) {
            let key = (item.0, item.1.clone());
            if let Some(old_id) = self.memory_to_id.insert(key.clone(), *id) {
                if let Err(error) = self.index.remove(old_id) {
                    storage.mark_vector_rehydrate_failed(&error.to_string())?;
                    crate::debug_log(&format!(
                        "old batch vector removal failed; project rehydrate required: {error}"
                    ));
                }
                self.id_to_memory.remove(&old_id);
            }
            self.id_to_memory.insert(*id, key.clone());
        }
        Ok(())
    }

    /// recall path: FTS candidates → allowlist → re-rank → candidates.
    pub fn build_embedding_candidates(
        &mut self,
        storage: &TursoStorage,
        shared: &dyn EmbeddingProvider,
        prompt: &str,
        fts_candidate_memories: &[(MemoryKind, String)],
    ) -> anyhow::Result<Vec<EmbeddingCandidate>> {
        let Some(query) = shared
            .embed_query(&[prompt.to_string()])
            .and_then(|v| v.into_iter().next())
        else {
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
            self.index
                .search(&query, fts_candidate_memories.len().max(1))?
        } else {
            self.index.search_with_allowlist(
                &query,
                fts_candidate_memories.len(),
                Some(&allowlist),
            )?
        };

        let mut candidates = Vec::new();
        for hit in hits {
            if hit.score <= 0.0 {
                continue; // matches the TS filter(embeddingScore > 0)
            }
            match self.id_to_memory.get(&hit.id).cloned() {
                Some((kind, memory_id)) => {
                    if let Some(candidate) =
                        Self::load_candidate(storage, kind, &memory_id, hit.score as f64)?
                    {
                        candidates.push(candidate);
                    } else {
                        self.remove_memory(kind, &memory_id);
                    }
                }
                None => self.remove_vector_id(hit.id),
            }
        }
        Ok(candidates)
    }

    /// Search the full project vector index, independent of FTS results.
    pub fn search_global_candidates(
        &mut self,
        storage: &TursoStorage,
        shared: &dyn EmbeddingProvider,
        prompt: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<EmbeddingCandidate>> {
        let Some(query) = shared
            .embed_query(&[prompt.to_string()])
            .and_then(|v| v.into_iter().next())
        else {
            return Ok(Vec::new());
        };
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let target = limit.min(self.index_len());
        let mut candidates = Vec::with_capacity(target);
        let mut seen = HashSet::new();
        let mut search_limit = target;
        for _ in 0..4 {
            let mut removed_stale = false;
            let hits = self.index.search(&query, search_limit)?;
            for hit in hits {
                if hit.score <= 0.0 || candidates.len() >= target {
                    continue;
                }
                match self.id_to_memory.get(&hit.id).cloned() {
                    Some((kind, memory_id)) => {
                        if let Some(candidate) =
                            Self::load_candidate(storage, kind, &memory_id, hit.score as f64)?
                        {
                            if seen.insert((kind, memory_id)) {
                                candidates.push(candidate);
                            }
                        } else {
                            self.remove_memory(kind, &memory_id);
                            removed_stale = true;
                        }
                    }
                    None => {
                        self.remove_vector_id(hit.id);
                        removed_stale = true;
                    }
                }
            }
            if !removed_stale || candidates.len() >= target {
                break;
            }
            search_limit = target.min(self.index_len());
            if search_limit == 0 {
                break;
            }
        }
        Ok(candidates)
    }

    fn load_candidate(
        storage: &TursoStorage,
        kind: MemoryKind,
        id: &str,
        score: f64,
    ) -> anyhow::Result<Option<EmbeddingCandidate>> {
        let candidate = match kind {
            MemoryKind::Working => storage.get_working(id)?.map(|row| EmbeddingCandidate {
                memory_kind: kind,
                memory_id: id.to_string(),
                content: row.content,
                project_scope: row.project_scope,
                consolidated: row.consolidated_at.is_some(),
                superseded: row.superseded_by.is_some(),
                valid_until: row.valid_until,
                score,
            }),
            MemoryKind::Episodic => storage.get_episodic(id)?.map(|row| EmbeddingCandidate {
                memory_kind: kind,
                memory_id: id.to_string(),
                content: row.content,
                project_scope: row.project_scope,
                consolidated: false,
                superseded: row.superseded_by.is_some(),
                valid_until: row.valid_until,
                score,
            }),
            _ => None,
        };
        Ok(candidate)
    }

    /// Number of vectors currently in the index (tests/diagnostics).
    pub fn index_len(&self) -> usize {
        self.index.len()
    }

    /// Remove a memory's vector and in-memory mappings, if present.
    pub fn remove_memory(&mut self, memory_kind: MemoryKind, memory_id: &str) {
        let key = (memory_kind, memory_id.to_string());
        if let Some(vector_id) = self.memory_to_id.remove(&key) {
            let _ = self.index.remove(vector_id);
            self.id_to_memory.remove(&vector_id);
        }
    }

    fn remove_vector_id(&mut self, vector_id: u64) {
        if let Some(key) = self.id_to_memory.remove(&vector_id) {
            if self.memory_to_id.get(&key) == Some(&vector_id) {
                self.memory_to_id.remove(&key);
            }
        }
        let _ = self.index.remove(vector_id);
    }
}

fn lock_unpoisoned<'a, T>(mutex: &'a Mutex<T>, label: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            mutex.clear_poison();
            crate::debug_log(&format!("{label} mutex poisoned; recovering guard"));
            poisoned.into_inner()
        }
    }
}
