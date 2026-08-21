//! turso_core 存储层集成测试：验证打开/建表/插入/查询链路。

use pi_memory_core::storage::turso::{TursoStorage, WorkingMemoryRow};
use pi_memory_core::storage::Storage;

fn temp_db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join("pi-memory-rust-test");
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir.join(name).to_string_lossy().into_owned()
}

#[test]
fn turso_open_init_insert_select_roundtrip() {
    let path = temp_db_path("turso-roundtrip.db");
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).expect("打开 turso 数据库失败");

    // 插入一条
    storage
        .insert_working(&WorkingMemoryRow {
            id: "wm-1".into(),
            content: "用户偏好使用中文回复".into(),
            session_id: "sess-1".into(),
            project_scope: "/tmp/demo".into(),
            importance: 0.8,
            memory_type: "preference".into(),
            created_at: "".into(),
        })
        .expect("插入失败");

    // 读回
    let row = storage.get_working("wm-1").expect("查询失败").expect("应存在");
    assert_eq!(row.content, "用户偏好使用中文回复");
    assert_eq!(row.importance, 0.8);
    assert_eq!(row.memory_type, "preference");

    // 计数
    assert_eq!(storage.count_working().expect("计数失败"), 1);

    // 诊断
    let diag = storage.diagnose().expect("诊断失败");
    assert_eq!(diag["backend"], "turso");
    assert_eq!(diag["working_memory_count"], 1);

    // 幂等 init
    let again = TursoStorage::open(&path).expect("再次打开失败");
    assert_eq!(again.count_working().expect("计数失败"), 1, "重复 init 不应清空数据");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_missing_row_returns_none() {
    let path = temp_db_path("turso-missing.db");
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).expect("打开失败");
    assert!(storage.get_working("nope").expect("查询失败").is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_tantivy_fts_match_search() {
    let path = temp_db_path("turso-fts.db");
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).expect("打开失败");
    storage
        .insert_working(&WorkingMemoryRow {
            id: "wm-fts-1".into(),
            content: "用户偏好使用中文回复" .into(),
            session_id: "sess-1".into(),
            project_scope: "/tmp/demo".into(),
            importance: 0.8,
            memory_type: "preference".into(),
            created_at: "".into(),
        })
        .expect("插入 1 失败");
    storage
        .insert_working(&WorkingMemoryRow {
            id: "wm-fts-2".into(),
            content: "项目使用 turbovec 向量索引".into(),
            session_id: "sess-1".into(),
            project_scope: "/tmp/demo".into(),
            importance: 0.5,
            memory_type: "fact".into(),
            created_at: "".into(),
        })
        .expect("插入 2 失败");

    // 关键词搜索：行为与 SQLite FTS5 unicode61 对齐——中文连续串是单个 token，
    // 完整 token 命中，子串不命中（TS 版同样如此）。
    // content: "项目使用 turbovec 向量索引" → tokens: [项目使用, turbovec, 向量索引]
    let hits = storage.search_working_fts("向量索引").expect("FTS 搜索失败");
    assert!(!hits.is_empty(), "完整 token「向量索引」应命中");
    assert_eq!(hits[0].0, "wm-fts-2");

    // 子串「向量」不是独立 token，应与 TS 版一致不命中
    let partial = storage.search_working_fts("向量").expect("FTS 搜索失败");
    assert!(partial.is_empty(), "子串「向量」不应命中（与 FTS5 行为一致）");

    // 不存在的词应无命中
    let none = storage.search_working_fts("不存在的关键词xyz").expect("FTS 搜索失败");
    assert!(none.is_empty(), "不应命中");

    let _ = std::fs::remove_file(&path);
}
