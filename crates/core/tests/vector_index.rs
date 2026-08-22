//! turbovec 向量索引集成测试。

use pi_memory_core::embedding_runtime::ProjectVectors;
use pi_memory_core::models::{MemoryEmbeddingRecord, MemoryKind, WorkingMemoryRecord};
use pi_memory_core::storage::TursoStorage;
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
    index.add(&[unit_vector(8, 0.0), unit_vector(8, 1.0)]).expect("add 失败");

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
        index.add(&[unit_vector(8, 0.0), unit_vector(8, 3.0)]).expect("add 失败");
    }
    // 读取：turbovec 的 sync 文件可被 IdMapIndex::load 恢复（由 VectorIndex 后续支持 load）
    let restored = turbovec::IdMapIndex::load(&path).expect("load 失败");
    assert_eq!(restored.len(), 2, "持久化索引应恢复 2 个向量");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn hydrate_rebuilds_project_vector_index() {
    let dir = std::env::temp_dir().join("pi-memory-rust-test-hydrate");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("memory.db");
    let db = TursoStorage::open(db_path.to_str().unwrap()).unwrap();
    db.insert_working(&WorkingMemoryRecord {
        id: "wm-1".into(),
        content: "事实: 项目正式采用 Rust".into(),
        embed_text: Some("事实: 项目正式采用 Rust".into()),
        source: Some("test".into()),
        session_id: "s1".into(),
        project_scope: "/proj/a".into(),
        importance: 0.5,
        memory_type: "working".into(),
        metadata: None,
        created_at: "2026-08-21T00:00:00.000Z".into(),
        updated_at: "2026-08-21T00:00:00.000Z".into(),
        consolidated_at: None,
        valid_until: None,
        superseded_by: None,
    }).unwrap();
    db.save_embedding(&MemoryEmbeddingRecord {
        memory_kind: MemoryKind::Working,
        memory_id: "wm-1".into(),
        embedding: vec![0.1; 384],
        model: "model".into(),
    }).unwrap();

    let mut vectors = ProjectVectors::new().unwrap();
    vectors.hydrate(&db).unwrap();
    // 直接断言索引状态（不依赖本地 ONNX 模型，CI 无模型时同样可验证 hydrate 恢复）
    assert_eq!(vectors.index_len(), 1, "hydrate 后应恢复 1 个向量");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clear_then_rebuild_drops_stale_vectors() {
    // 模拟 server 的 /memory clear 流程：清 DB（含 embeddings 表）后重建 ProjectVectors，
    // 避免内存索引残留导致 recall 注入已删除记忆。
    let dir = std::env::temp_dir().join("pi-memory-rust-test-clear");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("memory.db");
    let db = TursoStorage::open(db_path.to_str().unwrap()).unwrap();
    let ts = "2026-08-21T00:00:00.000Z".to_string();
    db.insert_working(&WorkingMemoryRecord {
        id: "wm-1".into(),
        content: "团队使用 Rust 开发后端服务".into(),
        embed_text: Some("团队使用 Rust 开发后端服务".into()),
        source: Some("test".into()),
        session_id: "s1".into(),
        project_scope: "/p".into(),
        importance: 0.5,
        memory_type: "working".into(),
        metadata: None,
        created_at: ts.clone(),
        updated_at: ts,
        consolidated_at: None,
        valid_until: None,
        superseded_by: None,
    })
    .unwrap();
    db.save_embedding(&MemoryEmbeddingRecord {
        memory_kind: MemoryKind::Working,
        memory_id: "wm-1".into(),
        embedding: vec![0.5; 384],
        model: "model".into(),
    })
    .unwrap();

    let mut vectors = ProjectVectors::new().unwrap();
    vectors.hydrate(&db).unwrap();
    assert_eq!(vectors.index_len(), 1, "clear 前应存在 1 个向量");

    // clear 只清 DB；server 侧随后重建 ProjectVectors（hydrate 空库 → 空索引）
    db.clear_project_memories("/p").unwrap();
    let mut rebuilt = ProjectVectors::new().unwrap();
    rebuilt.hydrate(&db).unwrap();
    assert_eq!(rebuilt.index_len(), 0, "clear 并重建后索引应为空（无残留）");
    let _ = std::fs::remove_dir_all(&dir);
}
