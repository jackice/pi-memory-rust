//! 英文 FTS 验证：排除分词器干扰，确认 USING fts 语法与查询链路本身可用。
use pi_memory_core::storage::turso::{TursoStorage, WorkingMemoryRow};

#[test]
fn turso_fts_english_match() {
    let dir = std::env::temp_dir().join("pi-memory-rust-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("turso-fts-en.db").to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).unwrap();
    for (id, content) in [
        ("en-1", "the project uses vector index for recall"),
        ("en-2", "the user prefers chinese language replies"),
    ] {
        storage.insert_working(&WorkingMemoryRow {
            id: id.into(), content: content.into(), session_id: "s1".into(),
            project_scope: "/tmp".into(), importance: 0.5,
            memory_type: "fact".into(), created_at: "".into(),
        }).unwrap();
    }
    let hits = storage.search_working_fts("vector").expect("英文 FTS 搜索失败");
    assert!(!hits.is_empty(), "英文「vector」应命中");
    assert_eq!(hits[0].0, "en-1", "应命中 en-1");
    let _ = std::fs::remove_file(&path);
}
