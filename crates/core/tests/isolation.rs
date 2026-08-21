//! 物理隔离验证：单进程多库，按 projectScope 独立 db 文件。
//! A 项目写入的数据 B 项目不可见，且库文件物理独立。

use pi_memory_core::embedding_runtime::{ProjectVectors, SharedEmbedder};
use pi_memory_core::paths;
use pi_memory_core::runtime::{build_recall_injection, retain_turn_memories};
use pi_memory_core::storage::{Storage, TursoStorage};

fn isolated_test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join("pi-memory-rust-isolation").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn project_scopes_resolve_to_distinct_db_files() {
    let data_dir = isolated_test_dir("paths");
    let a = paths::resolve_project_db_path(&data_dir, "/proj/a");
    let b = paths::resolve_project_db_path(&data_dir, "/proj/b");
    assert_ne!(a, b, "不同项目必须解析到不同库文件");
    assert!(a.parent().unwrap().to_string_lossy().contains("L3Byb2ovYQ"));
}

#[test]
fn isolation_blocks_cross_project_recall() {
    let data_dir = isolated_test_dir("recall");
    let shared = SharedEmbedder::new();

    // 项目 A 与项目 B 各自独立的库
    let db_a = paths::ensure_project_db_dir(&data_dir, "/proj/a").unwrap();
    let db_b = paths::ensure_project_db_dir(&data_dir, "/proj/b").unwrap();
    assert_ne!(db_a, db_b);
    let storage_a = TursoStorage::open(db_a.to_str().unwrap()).unwrap();
    let storage_b = TursoStorage::open(db_b.to_str().unwrap()).unwrap();
    let mut vectors_a = ProjectVectors::new().unwrap();
    let mut vectors_b = ProjectVectors::new().unwrap();

    // A 项目写入一条记忆
    retain_turn_memories(&storage_a, 1, "事实: 项目A使用 Rust 重写", "s1", "/proj/a", &shared, &mut vectors_a, None).unwrap();

    // B 项目查同样关键词：不应命中 A 的记忆
    let injection_b = build_recall_injection(&storage_b, "Rust 重写", "/proj/b", "s1", &shared, &mut vectors_b).unwrap();
    assert!(
        !injection_b.contains("项目A"),
        "B 项目不应召回 A 项目的记忆: {injection_b}"
    );

    // A 项目自己查：应命中
    let injection_a = build_recall_injection(&storage_a, "Rust 重写", "/proj/a", "s1", &shared, &mut vectors_a).unwrap();
    assert!(injection_a.contains("项目A"), "A 项目应召回自己的记忆: {injection_a}");

    // 物理文件独立
    assert_ne!(storage_a.db_path(), storage_b.db_path());
}

#[test]
fn same_project_shared_session_state() {
    let data_dir = isolated_test_dir("shared-session");
    let shared = SharedEmbedder::new();
    let db_a = paths::ensure_project_db_dir(&data_dir, "/proj/a").unwrap();
    let storage = TursoStorage::open(db_a.to_str().unwrap()).unwrap();
    let mut vectors = ProjectVectors::new().unwrap();

    // 同一项目多次 retain：turnIndex 幂等
    retain_turn_memories(&storage, 1, "记忆1", "s1", "/proj/a", &shared, &mut vectors, None).unwrap();
    retain_turn_memories(&storage, 1, "记忆1 重复", "s1", "/proj/a", &shared, &mut vectors, None).unwrap();
    assert_eq!(storage.count_working().unwrap(), 1, "同一 turn 不应重复 retain");
    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 0, "无事实前缀不提取");
}
