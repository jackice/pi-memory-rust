//! turbovec 向量索引封装（M2）。
//!
//! - `IdMapIndex`：u64 外部 id ↔ 内部 slot，O(1) 删除；
//! - allowlist 过滤：FTS 粗筛候选集合作精排（SIMD 内核内过滤）；
//! - 持久化：`sync` 增量写独立索引文件（DB BLOB 方案 M3 评估）。

use std::sync::Mutex;

use turbovec::IdMapIndex;

/// 向量维度（all-MiniLM-L6-v2 固定 384）。
pub const DEFAULT_DIM: usize = 384;
/// 量化位宽（4-bit 起步，spec 允许 2/3-bit 对照）。
pub const BIT_WIDTH: usize = 4;

/// 线程安全的向量索引（子进程单线程访问为主，Mutex 兜底）。
pub struct VectorIndex {
    index: Mutex<IdMapIndex>,
    next_id: Mutex<u64>,
    /// 持久化文件路径（None 表示未启用持久化）
    sync_path: Option<std::path::PathBuf>,
}

/// 搜索结果（外部位 id + 分数，inner product）。
#[derive(Debug, Clone)]
pub struct VectorHit {
    pub id: u64,
    pub score: f32,
}

impl VectorIndex {
    /// 新建索引。`sync_path` 提供时启用增量持久化（sync 每次变更后调用）。
    pub fn new(dim: usize, sync_path: Option<std::path::PathBuf>) -> anyhow::Result<Self> {
        let index = IdMapIndex::new(dim, BIT_WIDTH)
            .map_err(|e| anyhow::anyhow!("创建 turbovec 索引失败: {e:?}"))?;
        Ok(Self {
            index: Mutex::new(index),
            next_id: Mutex::new(0),
            sync_path,
        })
    }

    /// 追加一批向量，返回分配的外部 id（从 0 递增；调用方自行映射到记忆 id）。
    pub fn add(&self, vectors: &[Vec<f32>]) -> anyhow::Result<Vec<u64>> {
        if vectors.is_empty() {
            return Ok(Vec::new());
        }
        let dim = vectors[0].len();
        let mut flat = Vec::with_capacity(vectors.len() * dim);
        for v in vectors {
            anyhow::ensure!(v.len() == dim, "向量维度不一致");
            flat.extend_from_slice(v);
        }
        let mut next = self.next_id.lock().expect("锁中毒");
        let start = *next;
        let ids: Vec<u64> = (start..start + vectors.len() as u64).collect();
        *next += vectors.len() as u64;
        drop(next);

        let mut index = self.index.lock().expect("锁中毒");
        index
            .add_with_ids(&flat, &ids)
            .map_err(|e| anyhow::anyhow!("turbovec add 失败: {e:?}"))?;
        drop(index);
        self.sync_if_enabled()?;
        Ok(ids)
    }

    /// 使用已有外部 id 恢复一批向量，避免重启后映射漂移。
    pub fn add_with_ids(&self, vectors: &[Vec<f32>], ids: &[u64]) -> anyhow::Result<()> {
        anyhow::ensure!(vectors.len() == ids.len(), "向量与 id 数量不一致");
        if vectors.is_empty() {
            return Ok(());
        }
        let dim = vectors[0].len();
        let mut flat = Vec::with_capacity(vectors.len() * dim);
        for vector in vectors {
            anyhow::ensure!(vector.len() == dim, "向量维度不一致");
            flat.extend_from_slice(vector);
        }
        let mut index = self.index.lock().expect("锁中毒");
        index
            .add_with_ids(&flat, ids)
            .map_err(|e| anyhow::anyhow!("turbovec 恢复失败: {e:?}"))?;
        drop(index);
        let next = ids.iter().copied().max().map_or(0, |id| id.saturating_add(1));
        let mut next_id = self.next_id.lock().expect("锁中毒");
        *next_id = (*next_id).max(next);
        Ok(())
    }

    /// 删除（O(1) by id）。
    pub fn remove(&self, id: u64) -> anyhow::Result<()> {
        let removed = {
            let mut index = self.index.lock().expect("锁中毒");
            index.remove(id)
        };
        if !removed {
            anyhow::bail!("turbovec remove 失败：id {id} 不存在");
        }
        self.sync_if_enabled()
    }

    /// 查询：query 向量 vs 全量（top-k）。
    pub fn search(&self, query: &[f32], k: usize) -> anyhow::Result<Vec<VectorHit>> {
        self.search_with_allowlist(query, k, None)
    }

    /// 查询（allowlist 过滤）：只从允许的 id 集合中返回 top-k。
    pub fn search_with_allowlist(
        &self,
        query: &[f32],
        k: usize,
        allowlist: Option<&[u64]>,
    ) -> anyhow::Result<Vec<VectorHit>> {
        let index = self.index.lock().expect("锁中毒");
        let (scores, ids) = index
            .search_with_allowlist(query, k, allowlist)
            .map_err(|e| anyhow::anyhow!("turbovec 检索失败: {e:?}"))?;
        Ok(scores
            .iter()
            .zip(ids.iter())
            .map(|(&score, &id)| VectorHit { id, score })
            .collect())
    }

    /// 索引内向量数。
    pub fn len(&self) -> usize {
        self.index.lock().expect("锁中毒").len()
    }

    /// 下一个可分配的外部 id。
    pub(crate) fn next_id_hint(&self) -> u64 {
        *self.next_id.lock().expect("锁中毒")
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 同步持久化（启用时）。
    fn sync_if_enabled(&self) -> anyhow::Result<()> {
        if let Some(path) = &self.sync_path {
            let mut index = self.index.lock().expect("锁中毒");
            index
                .sync(path)
                .map_err(|e| anyhow::anyhow!("turbovec sync 失败: {e:?}"))?;
        }
        Ok(())
    }
}
