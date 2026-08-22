//! turso_core 存储层集成测试：CRUD、FTS、幂等、rebuild。

use pi_memory_core::models::{
    EpisodicMemoryRecord, FactRecord, InstructionRecord, MemoryEmbeddingRecord, MemoryKind,
    PreferenceRecord, TimelineEventRecord, WorkingMemoryRecord,
};
use pi_memory_core::storage::{Storage, TursoStorage};

fn temp_db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join("pi-memory-rust-test");
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir.join(name).to_string_lossy().into_owned()
}

fn wm(id: &str, content: &str, scope: &str) -> WorkingMemoryRecord {
    WorkingMemoryRecord {
        id: id.into(),
        content: content.into(),
        embed_text: None,
        source: None,
        session_id: "sess-1".into(),
        project_scope: scope.into(),
        importance: 0.5,
        memory_type: "fact".into(),
        metadata: None,
        created_at: "2026-08-21 00:00:00".into(),
        updated_at: "2026-08-21 00:00:00".into(),
        consolidated_at: None,
        valid_until: None,
        superseded_by: None,
    }
}

#[test]
fn turso_open_init_insert_select_roundtrip() {
    let path = temp_db_path("turso-roundtrip.db");
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).expect("打开 turso 数据库失败");

    storage
        .insert_working(&wm("wm-1", "用户偏好使用中文回复", "/tmp/demo"))
        .expect("插入失败");

    let row = storage.get_working("wm-1").expect("查询失败").expect("应存在");
    assert_eq!(row.content, "用户偏好使用中文回复");
    assert_eq!(row.importance, 0.5);

    assert_eq!(storage.count_working().expect("计数失败"), 1);

    // 幂等 init：重复打开不丢数据
    let again = TursoStorage::open(&path).expect("再次打开失败");
    assert_eq!(again.count_working().expect("计数失败"), 1);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_upsert_same_id_updates() {
    let path = temp_db_path("turso-upsert.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage.insert_working(&wm("wm-1", "旧内容", "/tmp/demo")).expect("首次插入失败");
    storage.insert_working(&wm("wm-1", "新内容", "/tmp/demo")).expect("更新失败");

    let row = storage.get_working("wm-1").expect("查询失败").expect("应存在");
    assert_eq!(row.content, "新内容", "upsert 应更新内容");
    assert_eq!(storage.count_working().expect("计数失败"), 1, "不应新增行");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_list_and_scope_filter() {
    let path = temp_db_path("turso-list.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage.insert_working(&wm("a", "A 项目记忆", "/proj/a")).unwrap();
    storage.insert_working(&wm("b", "B 项目记忆", "/proj/b")).unwrap();

    let list = storage.list_working_by_project("/proj/a").expect("列表失败");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "a");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_episodic_crud() {
    let path = temp_db_path("turso-episodic.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage
        .insert_episodic(&EpisodicMemoryRecord {
            id: "ep-1".into(),
            content: "会话摘要：讨论了向量索引方案".into(),
            summary_of: "session-xyz".into(),
            session_id: "sess-1".into(),
            project_scope: "/tmp/demo".into(),
            importance: 0.7,
            tier: 1,
            memory_type: "summary".into(),
            metadata: None,
            created_at: "2026-08-21 00:00:00".into(),
            updated_at: "2026-08-21 00:00:00".into(),
            degraded_at: None,
            valid_until: None,
            superseded_by: None,
        })
        .expect("插入 episodic 失败");

    assert_eq!(storage.count_episodic().expect("计数失败"), 1);
    let list = storage.list_episodic_by_project("/tmp/demo").expect("列表失败");
    assert_eq!(list[0].summary_of, "session-xyz");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_structured_tables_crud() {
    let path = temp_db_path("turso-structured.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage
        .insert_facts(&[FactRecord {
            fact_id: "f-1".into(),
            subject: "用户".into(),
            predicate: "偏好".into(),
            object: "中文".into(),
            confidence: 0.9,
            source_memory_id: Some("wm-1".into()),
            session_id: "sess-1".into(),
            project_scope: "/p".into(),
            timestamp: "2026-08-21".into(),
        }])
        .unwrap();
    storage
        .insert_preferences(&[PreferenceRecord {
            id: "p-1".into(),
            preference: "回复使用中文".into(),
            topic: Some("language".into()),
            context_snippet: None,
            source_memory_id: None,
            session_id: "sess-1".into(),
            project_scope: "/p".into(),
            created_at: "2026-08-21".into(),
        }])
        .unwrap();
    storage
        .insert_instructions(&[InstructionRecord {
            id: "i-1".into(),
            instruction: "提交前运行测试".into(),
            topic: Some("workflow".into()),
            active: true,
            context_snippet: None,
            source_memory_id: None,
            session_id: "sess-1".into(),
            project_scope: "/p".into(),
            created_at: "2026-08-21".into(),
        }])
        .unwrap();
    storage
        .insert_timeline_events(&[TimelineEventRecord {
            event_id: "t-1".into(),
            date: "2026-08-21".into(),
            description: "开始 Rust 重写".into(),
            source: Some("session".into()),
            source_memory_id: None,
            session_id: "sess-1".into(),
            project_scope: "/p".into(),
            created_at: "2026-08-21".into(),
        }])
        .unwrap();

    assert_eq!(storage.count_by_session("facts", "sess-1").unwrap(), 1);
    assert_eq!(storage.count_by_session("preferences", "sess-1").unwrap(), 1);
    assert_eq!(storage.count_by_session("instructions", "sess-1").unwrap(), 1);
    assert_eq!(storage.count_by_session("timeline", "sess-1").unwrap(), 1);
    assert_eq!(storage.count_by_session("facts", "other-session").unwrap(), 0);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn structured_search_filters_by_project_scope() {
    let path = temp_db_path("turso-scope.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    // 项目 A 与项目 B 各一条 facts
    storage
        .insert_facts(&[FactRecord {
            fact_id: "f-a".into(),
            subject: "项目A".into(),
            predicate: "使用".into(),
            object: "Rust".into(),
            confidence: 0.9,
            source_memory_id: None,
            session_id: "sess-a".into(),
            project_scope: "/proj/a".into(),
            timestamp: "2026-08-21".into(),
        }])
        .unwrap();
    storage
        .insert_facts(&[FactRecord {
            fact_id: "f-b".into(),
            subject: "项目B".into(),
            predicate: "使用".into(),
            object: "Rust".into(),
            confidence: 0.9,
            source_memory_id: None,
            session_id: "sess-b".into(),
            project_scope: "/proj/b".into(),
            timestamp: "2026-08-21".into(),
        }])
        .unwrap();

    let in_a = storage.search_facts_like("Rust", "/proj/a").unwrap();
    assert_eq!(in_a.len(), 1, "项目 A 只应命中自己的 facts");
    assert_eq!(in_a[0].fact_id, "f-a");
    assert_eq!(in_a[0].project_scope, "/proj/a");

    // 旧库迁移数据（project_scope 为空串）兼容可见，不因隔离而消失
    storage
        .insert_facts(&[FactRecord {
            fact_id: "f-legacy".into(),
            subject: "旧数据".into(),
            predicate: "标记".into(),
            object: "legacy".into(),
            confidence: 0.9,
            source_memory_id: None,
            session_id: "sess-legacy".into(),
            project_scope: String::new(),
            timestamp: "2026-08-21".into(),
        }])
        .unwrap();
    let in_a_legacy = storage.search_facts_like("legacy", "/proj/a").unwrap();
    assert_eq!(in_a_legacy.len(), 1, "空 scope 旧数据应兼容可见");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_embedding_roundtrip() {
    let path = temp_db_path("turso-embedding.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage
        .save_embedding(&MemoryEmbeddingRecord {
            memory_kind: MemoryKind::Working,
            memory_id: "wm-1".into(),
            embedding: vec![0.1, 0.2, 0.3],
            model: "test-model".into(),
        })
        .unwrap();

    let loaded = storage
        .get_embedding(MemoryKind::Working, "wm-1")
        .expect("查询失败")
        .expect("应存在");
    assert_eq!(loaded.embedding, vec![0.1, 0.2, 0.3]);
    assert_eq!(loaded.model, "test-model");

    // 覆盖更新
    storage
        .save_embedding(&MemoryEmbeddingRecord {
            memory_kind: MemoryKind::Working,
            memory_id: "wm-1".into(),
            embedding: vec![0.9],
            model: "test-model".into(),
        })
        .unwrap();
    let updated = storage.get_embedding(MemoryKind::Working, "wm-1").unwrap().unwrap();
    assert_eq!(updated.embedding, vec![0.9]);

    // 不存在的返回 None
    assert!(storage.get_embedding(MemoryKind::Episodic, "nope").unwrap().is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_tantivy_fts_match_search() {
    let path = temp_db_path("turso-fts.db");
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).expect("打开失败");
    storage.insert_working(&wm("wm-fts-1", "项目使用 turbovec 向量索引", "/tmp/demo")).unwrap();
    storage.insert_working(&wm("wm-fts-2", "另一条无关记忆", "/tmp/demo")).unwrap();

    // 完整 token 命中；子串不命中（与 SQLite FTS5 unicode61 行为一致）
    let hits = storage.search_working_fts("向量索引", "/tmp/demo").expect("FTS 搜索失败");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0.id, "wm-fts-1");

    let partial = storage.search_working_fts("向量", "/tmp/demo").expect("FTS 搜索失败");
    assert!(partial.is_empty(), "子串不应命中（与 FTS5 一致）");

    // scope 过滤
    let other_scope = storage.search_working_fts("向量索引", "/other").expect("FTS 搜索失败");
    assert!(other_scope.is_empty(), "其他 scope 不应命中");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_fts_update_sync() {
    let path = temp_db_path("turso-fts-update.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    // 插入旧内容
    storage.insert_working(&wm("wm-u", "旧内容没有关键词", "/tmp/demo")).unwrap();
    assert!(storage.search_working_fts("keyword", "/tmp/demo").unwrap().is_empty());

    // upsert 为新内容（含关键词）
    storage.insert_working(&wm("wm-u", "新内容包含 keyword 关键词", "/tmp/demo")).unwrap();
    let hits = storage.search_working_fts("keyword", "/tmp/demo").expect("FTS 搜索失败");
    assert!(!hits.is_empty(), "Tantivy FTS 应同步 upsert 后的内容");
    assert_eq!(hits[0].0.id, "wm-u");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_rebuild_fts() {
    let path = temp_db_path("turso-rebuild.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage.insert_working(&wm("wm-r", "rebuild 后仍可命中 keyword", "/tmp/demo")).unwrap();
    storage.rebuild_fts().expect("rebuild 失败");

    let hits = storage.search_working_fts("keyword", "/tmp/demo").expect("FTS 搜索失败");
    assert!(!hits.is_empty(), "rebuild 后应仍能命中");
    assert_eq!(hits[0].0.id, "wm-r");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_diagnose() {
    let path = temp_db_path("turso-diagnose.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");
    storage.insert_working(&wm("wm-1", "x", "/tmp/demo")).unwrap();

    let diag = storage.diagnose().expect("诊断失败");
    assert_eq!(diag["backend"], "turso");
    assert_eq!(diag["fts"], "tantivy");
    assert_eq!(diag["working_memory_count"], 1);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn concurrent_write_waits_with_busy_timeout() {
    // 两个连接（模拟两个 pi 会话共享同一项目 DB）：libSQL WAL + busy_timeout 下，
    // 短写冲突应等待对方提交后成功，而不是立即报错。
    let path = temp_db_path("turso-busy.db");
    let _ = std::fs::remove_file(&path);
    let s1 = TursoStorage::open(&path).expect("open s1");
    let s2 = TursoStorage::open(&path).expect("open s2");

    // s1 持写锁的事务
    s1.begin_tx().expect("begin");
    s1.insert_working(&wm("wm-1", "content a", "/p")).expect("insert 1");

    // s2 在 s1 事务未提交时写入：busy_timeout 内等待，s1 提交后成功
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        s2.insert_working(&wm("wm-2", "content b", "/p")).map(|_| ())
    });
    std::thread::sleep(std::time::Duration::from_millis(600));
    s1.commit_tx().expect("commit");

    let result = t.join().expect("join");
    assert!(result.is_ok(), "busy_timeout 下并发写应等待成功: {result:?}");
    assert_eq!(s1.list_working_by_project("/p").expect("list").len(), 2, "两次写入都应落库");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn large_multi_transaction_writes_are_stable() {
    // 回归防护：多事务大写入（facts 1 万条 + working 1 万条，drop_fts 后）在默认配置下
    // 必须不 panic、不卡死、数据完整。若 libSQL multiprocess WAL 被误开启，
    // 此场景会触发 "shared WAL frame ids must increase monotonically" panic。
    let path = temp_db_path("turso-large-tx.db");
    let _ = std::fs::remove_file(&path);
    let db = TursoStorage::open(&path).expect("open");
    db.drop_fts().expect("drop fts"); // 规避 FTS 批量写入悬崖（与 migrate/bench 同一策略）

    // 事务 1：facts 1 万条
    let mut facts = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        facts.push(FactRecord {
            fact_id: format!("f{i}"), subject: format!("用户{i}"), predicate: "使用".into(),
            object: format!("特性{i}"), confidence: 0.8, source_memory_id: None,
            session_id: "s1".into(), project_scope: "/p".into(), timestamp: "2026-08-21".into(),
        });
    }
    db.begin_tx().expect("begin 1");
    db.insert_facts(&facts).expect("insert facts");
    db.commit_tx().expect("commit 1");

    // 事务 2：working 1 万条
    db.begin_tx().expect("begin 2");
    let ts = "2026-08-21T00:00:00.000Z".to_string();
    for i in 0..10_000 {
        db.insert_working(&WorkingMemoryRecord {
            id: format!("w{i}"), content: format!("alpha 内容 {i:05}"), embed_text: None,
            source: Some("t".into()), session_id: "s1".into(), project_scope: "/p".into(),
            importance: 0.5, memory_type: "working".into(), metadata: None,
            created_at: ts.clone(), updated_at: ts.clone(), consolidated_at: None,
            valid_until: None, superseded_by: None,
        }).expect("insert working");
    }
    db.commit_tx().expect("commit 2");

    // 数据完整 + FTS 重建后可搜
    assert_eq!(db.list_all_facts().expect("facts").len(), 10_000);
    assert_eq!(db.list_working_by_project("/p").expect("working").len(), 10_000);
    db.rebuild_fts().expect("rebuild fts");
    let hits = db.search_working_fts("alpha", "/p").expect("fts");
    assert!(hits.len() >= 10_000, "FTS 应命中全部（实际 {}）", hits.len());

    let _ = std::fs::remove_file(&path);
}
