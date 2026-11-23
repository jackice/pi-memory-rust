//! turbovec vector index wrapper (M2).
//!
//! - `IdMapIndex`: u64 external id ↔ internal slot, O(1) removal;
//! - allowlist filtering: re-rank the FTS coarse candidates (filtered inside the SIMD kernel);
//! - persistence: `sync` incrementally writes a standalone index file (DB BLOB approach evaluated in M3).

use std::sync::{Mutex, MutexGuard};

use turbovec::IdMapIndex;

/// Vector dimension (fixed 384 for all-MiniLM-L6-v2).
pub const DEFAULT_DIM: usize = 384;
/// Quantization bit width (4-bit baseline; spec allows 2/3-bit comparisons).
pub const BIT_WIDTH: usize = 4;

/// Thread-safe vector index (mostly single-threaded in the child process; Mutex as a backstop).
pub struct VectorIndex {
    index: Mutex<IdMapIndex>,
    next_id: Mutex<u64>,
    dim: usize,
    /// Persistence file path (None means persistence is disabled)
    sync_path: Option<std::path::PathBuf>,
}

/// Search result (external id + score, inner product).
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: u64,
    pub score: f32,
}

impl VectorIndex {
    /// Create a new index. Incremental persistence is enabled when `sync_path` is provided (sync called after each change).
    pub fn new(dim: usize, sync_path: Option<std::path::PathBuf>) -> anyhow::Result<Self> {
        let index = IdMapIndex::new(dim, BIT_WIDTH)
            .map_err(|e| anyhow::anyhow!("failed to create turbovec index: {e:?}"))?;
        Ok(Self {
            index: Mutex::new(index),
            next_id: Mutex::new(0),
            dim,
            sync_path,
        })
    }

    /// Append a batch of vectors, returning the allocated external ids (incrementing from 0; the caller maps them to memory ids).
    pub fn add(&self, vectors: &[Vec<f32>]) -> anyhow::Result<Vec<u64>> {
        if vectors.is_empty() {
            return Ok(Vec::new());
        }
        let mut flat = Vec::with_capacity(vectors.len() * self.dim);
        for v in vectors {
            self.validate_vector(v)?;
            flat.extend_from_slice(v);
        }
        let mut next = lock_unpoisoned(&self.next_id, "vector id counter");
        let start = *next;
        let end = start
            .checked_add(vectors.len() as u64)
            .ok_or_else(|| anyhow::anyhow!("vector id counter overflow"))?;
        let ids: Vec<u64> = (start..end).collect();
        *next = end;
        drop(next);

        let mut index = lock_unpoisoned(&self.index, "vector index");
        index
            .add_with_ids(&flat, &ids)
            .map_err(|e| anyhow::anyhow!("turbovec add failed: {e:?}"))?;
        drop(index);
        self.sync_if_enabled()?;
        Ok(ids)
    }

    /// Restore a batch of vectors with existing external ids so mappings survive restarts.
    pub fn add_with_ids(&self, vectors: &[Vec<f32>], ids: &[u64]) -> anyhow::Result<()> {
        anyhow::ensure!(vectors.len() == ids.len(), "vector and id count mismatch");
        if vectors.is_empty() {
            return Ok(());
        }
        let mut flat = Vec::with_capacity(vectors.len() * self.dim);
        for vector in vectors {
            self.validate_vector(vector)?;
            flat.extend_from_slice(vector);
        }
        let mut index = lock_unpoisoned(&self.index, "vector index");
        index
            .add_with_ids(&flat, ids)
            .map_err(|e| anyhow::anyhow!("turbovec restore failed: {e:?}"))?;
        drop(index);
        let next = ids
            .iter()
            .copied()
            .max()
            .map_or(0, |id| id.saturating_add(1));
        let mut next_id = lock_unpoisoned(&self.next_id, "vector id counter");
        *next_id = (*next_id).max(next);
        Ok(())
    }

    /// Remove (O(1) by id).
    pub fn remove(&self, id: u64) -> anyhow::Result<()> {
        let removed = {
            let mut index = lock_unpoisoned(&self.index, "vector index");
            index.remove(id)
        };
        if !removed {
            anyhow::bail!("turbovec remove failed: id {id} does not exist");
        }
        self.sync_if_enabled()
    }

    /// Query: query vector vs the full index (top-k).
    pub fn search(&self, query: &[f32], k: usize) -> anyhow::Result<Vec<VectorHit>> {
        self.search_with_allowlist(query, k, None)
    }

    /// Query (allowlist filtered): only return top-k from the allowed id set.
    pub fn search_with_allowlist(
        &self,
        query: &[f32],
        k: usize,
        allowlist: Option<&[u64]>,
    ) -> anyhow::Result<Vec<VectorHit>> {
        self.validate_vector(query)?;
        let index = lock_unpoisoned(&self.index, "vector index");
        let (scores, ids) = index
            .search_with_allowlist(query, k, allowlist)
            .map_err(|e| anyhow::anyhow!("turbovec search failed: {e:?}"))?;
        Ok(scores
            .iter()
            .zip(ids.iter())
            .map(|(&score, &id)| VectorHit { id, score })
            .collect())
    }

    /// Number of vectors in the index.
    pub fn len(&self) -> usize {
        lock_unpoisoned(&self.index, "vector index").len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sync persistence (when enabled).
    fn sync_if_enabled(&self) -> anyhow::Result<()> {
        if let Some(path) = &self.sync_path {
            let mut index = lock_unpoisoned(&self.index, "vector index");
            index
                .sync(path)
                .map_err(|e| anyhow::anyhow!("turbovec sync failed: {e:?}"))?;
        }
        Ok(())
    }

    fn validate_vector(&self, vector: &[f32]) -> anyhow::Result<()> {
        anyhow::ensure!(
            vector.len() == self.dim,
            "vector dimension mismatch: expected {}, got {}",
            self.dim,
            vector.len()
        );
        anyhow::ensure!(
            vector.iter().all(|value| value.is_finite()),
            "vector contains a non-finite value"
        );
        Ok(())
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
