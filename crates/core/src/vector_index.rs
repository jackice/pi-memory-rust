//! turbovec vector index wrapper (M2).
//!
//! - `IdMapIndex`: u64 external id ↔ internal slot, O(1) removal;
//! - allowlist filtering: re-rank the FTS coarse candidates (filtered inside the SIMD kernel);
//! - persistence: `sync` incrementally writes a standalone index file (DB BLOB approach evaluated in M3).

use std::sync::Mutex;

use turbovec::IdMapIndex;

/// Vector dimension (fixed 384 for all-MiniLM-L6-v2).
pub const DEFAULT_DIM: usize = 384;
/// Quantization bit width (4-bit baseline; spec allows 2/3-bit comparisons).
pub const BIT_WIDTH: usize = 4;

/// Thread-safe vector index (mostly single-threaded in the child process; Mutex as a backstop).
pub struct VectorIndex {
    index: Mutex<IdMapIndex>,
    next_id: Mutex<u64>,
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
            sync_path,
        })
    }

    /// Append a batch of vectors, returning the allocated external ids (incrementing from 0; the caller maps them to memory ids).
    pub fn add(&self, vectors: &[Vec<f32>]) -> anyhow::Result<Vec<u64>> {
        if vectors.is_empty() {
            return Ok(Vec::new());
        }
        let dim = vectors[0].len();
        let mut flat = Vec::with_capacity(vectors.len() * dim);
        for v in vectors {
            anyhow::ensure!(v.len() == dim, "vector dimension mismatch");
            flat.extend_from_slice(v);
        }
        let mut next = self.next_id.lock().expect("mutex poisoned");
        let start = *next;
        let ids: Vec<u64> = (start..start + vectors.len() as u64).collect();
        *next += vectors.len() as u64;
        drop(next);

        let mut index = self.index.lock().expect("mutex poisoned");
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
        let dim = vectors[0].len();
        let mut flat = Vec::with_capacity(vectors.len() * dim);
        for vector in vectors {
            anyhow::ensure!(vector.len() == dim, "vector dimension mismatch");
            flat.extend_from_slice(vector);
        }
        let mut index = self.index.lock().expect("mutex poisoned");
        index
            .add_with_ids(&flat, ids)
            .map_err(|e| anyhow::anyhow!("turbovec restore failed: {e:?}"))?;
        drop(index);
        let next = ids.iter().copied().max().map_or(0, |id| id.saturating_add(1));
        let mut next_id = self.next_id.lock().expect("mutex poisoned");
        *next_id = (*next_id).max(next);
        Ok(())
    }

    /// Remove (O(1) by id).
    pub fn remove(&self, id: u64) -> anyhow::Result<()> {
        let removed = {
            let mut index = self.index.lock().expect("mutex poisoned");
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
        let index = self.index.lock().expect("mutex poisoned");
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
        self.index.lock().expect("mutex poisoned").len()
    }

    /// Next allocatable external id.
    pub(crate) fn next_id_hint(&self) -> u64 {
        *self.next_id.lock().expect("mutex poisoned")
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sync persistence (when enabled).
    fn sync_if_enabled(&self) -> anyhow::Result<()> {
        if let Some(path) = &self.sync_path {
            let mut index = self.index.lock().expect("mutex poisoned");
            index
                .sync(path)
                .map_err(|e| anyhow::anyhow!("turbovec sync failed: {e:?}"))?;
        }
        Ok(())
    }
}

