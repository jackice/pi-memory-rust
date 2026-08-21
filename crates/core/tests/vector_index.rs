//! turbovec 向量索引集成测试。

use pi_memory_core::vector_index::VectorIndex;

fn unit_vector(dim: usize, seed: f32) -> Vec<f32> {
    // 构造稀疏 one-hot 风格向量（保证方向差异明确）
    let mut v = vec![0f32; dim];
    v[(seed as usize) % dim] = 1.0;
    v
}

#[test]
fn add_search_basic() {
    let index = VectorIndex::new(8, None).expect("创建索引失败");
    let vectors = vec![unit_vector(8, 0.0), unit_vector(8, 1.0), unit_vector(8, 2.0)];
    let ids = index.add(&vectors).expect("add 失败");
    assert_eq!(ids, vec![0, 1, 2]);

    // 查询与 id=0 相同方向 → top1 应为 0
    let hits = index.search(&unit_vector(8, 0.0), 1).expect("检索失败");
    assert_eq!(hits[0].id, 0);
    assert_eq!(index.len(), 3);
}

#[test]
fn allowlist_filters_results() {
    let index = VectorIndex::new(8, None).expect("创建索引失败");
    let vectors = vec![unit_vector(8, 0.0), unit_vector(8, 1.0), unit_vector(8, 2.0)];
    index.add(&vectors).expect("add 失败");

    // 只允许 id=1：查询 id=0 方向时只能命中 1（次优），而不是 0
    let hits = index.search_with_allowlist(&unit_vector(8, 0.0), 3, Some(&[1])).expect("检索失败");
    assert_eq!(hits.len(), 1, "allowlist 外不应返回");
    assert_eq!(hits[0].id, 1);
}

#[test]
fn remove_omits_vector() {
    let index = VectorIndex::new(8, None).expect("创建索引失败");
    index.add(&vec![unit_vector(8, 0.0), unit_vector(8, 1.0)]).expect("add 失败");

    index.remove(0).expect("remove 失败");
    let hits = index.search(&unit_vector(8, 0.0), 1).expect("检索失败");
    assert_eq!(hits[0].id, 1, "删除后不应命中 0");
}

#[test]
fn sync_persistence_roundtrip() {
    let dir = std::env::temp_dir().join("pi-memory-rust-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("turbovec-sync.tvim");
    let _ = std::fs::remove_file(&path);

    // 写入
    {
        let index = VectorIndex::new(8, Some(path.clone())).expect("创建索引失败");
        index.add(&vec![unit_vector(8, 0.0), unit_vector(8, 3.0)]).expect("add 失败");
    }
    // 读取：turbovec 的 sync 文件可被 IdMapIndex::load 恢复（由 VectorIndex 后续支持 load）
    let restored = turbovec::IdMapIndex::load(&path).expect("load 失败");
    assert_eq!(restored.len(), 2, "持久化索引应恢复 2 个向量");

    let _ = std::fs::remove_file(&path);
}
