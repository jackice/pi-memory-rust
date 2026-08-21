//! 迁移工具测试：旧库（SQLite 兼容格式）→ 新库，数据完整 + FTS 可搜。

use pi_memory_core::models::{MemoryEmbeddingRecord, MemoryKind, WorkingMemoryRecord};
use pi_memory_core::storage::{Storage, TursoStorage};

fn temp_db(name: &str) -> String {
    let dir = std::env::temp_dir().join("pi-memory-rust-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name).to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    path
}

fn wm(id: &str, content: &str, scope: &str) -> WorkingMemoryRecord {
    WorkingMemoryRecord {
        id: id.into(), content: content.into(), embed_text: Some(content.into()), source: None,
        session_id: "s1".into(), project_scope: scope.into(), importance: 0.5,
        memory_type: "working".into(), metadata: None,
        created_at: "2026-08-19T03:00:00.000Z".into(), updated_at: "2026-08-19T03:00:00.000Z".into(),
        consolidated_at: None, valid_until: None, superseded_by: None,
    }
}

#[test]
fn migrate_old_db_preserves_data_and_fts() {
    // 1. 造旧库（模拟 TS 版 pi-memory 数据）
    let old = temp_db("migrate-old.db");
    let old_storage = TursoStorage::open(&old).expect("旧库打开失败");
    old_storage.insert_working(&wm("wm-1", "事实: 首版正式采用本地 SQLite", "/proj/demo")).unwrap();
    old_storage.insert_working(&wm("wm-2", "项目使用 turbovec 向量索引", "/proj/demo")).unwrap();
    old_storage.save_embedding(&MemoryEmbeddingRecord {
        memory_kind: MemoryKind::Working, memory_id: "wm-1".into(),
        embedding: vec![0.1, 0.2], model: "old-model".into(),
    }).unwrap();
    drop(old_storage);

    // 2. 迁入新库
    let new = temp_db("migrate-new.db");
    let new_storage = TursoStorage::open(&new).expect("新库打开失败");
    let stats = new_storage.migrate_from(&old).expect("迁移失败");
    assert_eq!(stats["working"], 2, "working 记忆应完整迁移");
    assert_eq!(stats["embeddings"], 1, "embedding 应迁移");

    // 3. 数据完整性
    let row = new_storage.get_working("wm-1").expect("查询失败").expect("wm-1 应存在");
    assert_eq!(row.content, "事实: 首版正式采用本地 SQLite");
    assert_eq!(row.project_scope, "/proj/demo");

    // 4. FTS 可搜（Tantivy 索引重建后）
    let hits = new_storage.search_working_fts("turbovec", "/proj/demo").expect("FTS 失败");
    assert!(!hits.is_empty(), "迁移后 FTS 应可命中");
    assert_eq!(hits[0].0.id, "wm-2");

    // 5. 结构化候选可用（facts 表由 wm-1 的内容提取？迁移只搬表，不做提取——验证原表数据）
    // 说明：迁移保留原样，结构化提取在 retain 时做；此处仅验证 count
    let diag = new_storage.diagnose().unwrap();
    assert_eq!(diag["working_memory_count"], 2);

    let _ = std::fs::remove_file(&old);
    let _ = std::fs::remove_file(&new);
}
