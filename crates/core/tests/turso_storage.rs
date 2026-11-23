//! turso_core 存储层集成测试：CRUD、FTS、幂等、rebuild。

use pi_memory_core::embedding_runtime::ProjectVectors;
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

    let row = storage
        .get_working("wm-1")
        .expect("查询失败")
        .expect("应存在");
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

    storage.begin_tx().expect("begin upsert tx");
    storage
        .insert_working(&wm("wm-1", "旧内容", "/tmp/demo"))
        .expect("首次插入失败");
    storage
        .insert_working(&wm("wm-1", "新内容", "/tmp/demo"))
        .expect("更新失败");
    storage.commit_tx().expect("commit upsert tx");

    let row = storage
        .get_working("wm-1")
        .expect("查询失败")
        .expect("应存在");
    assert_eq!(row.content, "新内容", "upsert 应更新内容");
    assert_eq!(storage.count_working().expect("计数失败"), 1, "不应新增行");

    drop(storage);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_list_and_scope_filter() {
    let path = temp_db_path("turso-list.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage
        .insert_working(&wm("a", "A 项目记忆", "/proj/a"))
        .unwrap();
    storage
        .insert_working(&wm("b", "B 项目记忆", "/proj/b"))
        .unwrap();

    let list = storage
        .list_working_by_project("/proj/a")
        .expect("列表失败");
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
    let list = storage
        .list_episodic_by_project("/tmp/demo")
        .expect("列表失败");
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
    assert_eq!(
        storage.count_by_session("preferences", "sess-1").unwrap(),
        1
    );
    assert_eq!(
        storage.count_by_session("instructions", "sess-1").unwrap(),
        1
    );
    assert_eq!(storage.count_by_session("timeline", "sess-1").unwrap(), 1);
    assert_eq!(
        storage.count_by_session("facts", "other-session").unwrap(),
        0
    );

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
fn clear_scope_removes_only_target_memories_and_persists_dirty_state() {
    let path = temp_db_path("clear-scope-isolation.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");
    for (id, scope) in [("wm-a", "/project/a"), ("wm-b", "/project/b")] {
        storage
            .insert_working(&wm(id, "scope isolation memory", scope))
            .unwrap();
        storage
            .insert_episodic(&EpisodicMemoryRecord {
                id: format!("ep-{id}"),
                content: "scope isolation episode".into(),
                summary_of: id.into(),
                session_id: "sess-1".into(),
                project_scope: scope.into(),
                importance: 0.7,
                tier: 1,
                memory_type: "summary".into(),
                metadata: None,
                created_at: "2026-08-21T00:00:00Z".into(),
                updated_at: "2026-08-21T00:00:00Z".into(),
                degraded_at: None,
                valid_until: None,
                superseded_by: None,
            })
            .unwrap();
        storage
            .insert_facts(&[FactRecord {
                fact_id: format!("fact-{id}"),
                subject: "project".into(),
                predicate: "uses".into(),
                object: id.into(),
                confidence: 0.9,
                source_memory_id: Some(id.into()),
                session_id: "sess-1".into(),
                project_scope: scope.into(),
                timestamp: "2026-08-21".into(),
            }])
            .unwrap();
        storage
            .save_embedding(&MemoryEmbeddingRecord {
                memory_kind: MemoryKind::Working,
                memory_id: id.into(),
                embedding: vec![0.1; 384],
                model: pi_memory_core::embedding_runtime::active_model_id(),
            })
            .unwrap();
    }

    storage.clear_project_memories("/project/a").unwrap();

    assert_eq!(storage.count_working_by_project("/project/a").unwrap(), 0);
    assert_eq!(storage.count_episodic_by_project("/project/a").unwrap(), 0);
    assert_eq!(storage.count_working_by_project("/project/b").unwrap(), 1);
    assert_eq!(storage.count_episodic_by_project("/project/b").unwrap(), 1);
    assert_eq!(storage.list_all_facts().unwrap().len(), 1);
    assert_eq!(
        storage.get_embedding(MemoryKind::Working, "wm-a").unwrap(),
        None
    );
    assert!(storage
        .get_embedding(MemoryKind::Working, "wm-b")
        .unwrap()
        .is_some());
    assert!(storage.fts_rebuild_needed().unwrap());
    assert!(storage.vector_rehydrate_needed().unwrap());
    let diagnose = storage.diagnose().unwrap();
    assert_eq!(diagnose["index_state"], "partial");

    drop(storage);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn consolidation_claims_are_disjoint_and_commit_exact_summary_sets() {
    let path = temp_db_path("consolidation-disjoint-claims.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open storage");
    storage
        .insert_working(&wm("claim-a", "first working memory", "/project/claims"))
        .unwrap();
    storage
        .insert_working(&wm("claim-b", "second working memory", "/project/claims"))
        .unwrap();

    let (claims_a, candidates_a) = storage
        .claim_working_lease("sess-1", "/project/claims", "owner-a", 60_000, 1)
        .unwrap();
    let (claims_b, candidates_b) = storage
        .claim_working_lease("sess-1", "/project/claims", "owner-b", 60_000, 10)
        .unwrap();
    assert_eq!(candidates_a, 1);
    assert_eq!(candidates_b, 1);
    assert_eq!(claims_a.len(), 1);
    assert_eq!(claims_b.len(), 1);
    assert_ne!(claims_a[0].id, claims_b[0].id);

    for (owner, claims, episodic_id) in [
        ("owner-a", &claims_a, "summary-a"),
        ("owner-b", &claims_b, "summary-b"),
    ] {
        let summary_ids = claims
            .iter()
            .map(|claim| claim.id.clone())
            .collect::<Vec<_>>();
        let episodic = EpisodicMemoryRecord {
            id: episodic_id.into(),
            content: format!("summary for {}", summary_ids[0]),
            summary_of: serde_json::to_string(&summary_ids).unwrap(),
            session_id: "sess-1".into(),
            project_scope: "/project/claims".into(),
            importance: 0.5,
            tier: 1,
            memory_type: "summary".into(),
            metadata: None,
            created_at: "2026-08-21T00:00:00Z".into(),
            updated_at: "2026-08-21T00:00:00Z".into(),
            degraded_at: None,
            valid_until: None,
            superseded_by: None,
        };
        assert!(storage
            .commit_working_lease(claims, owner, &episodic)
            .unwrap());
    }

    let episodic = storage.list_episodic_by_project("/project/claims").unwrap();
    assert_eq!(episodic.len(), 2);
    for row in episodic {
        let summary_ids: Vec<String> = serde_json::from_str(&row.summary_of).unwrap();
        assert_eq!(summary_ids.len(), 1);
        assert!(row.content.contains(&summary_ids[0]));
    }
    assert_eq!(
        storage
            .count_unconsolidated("sess-1", "/project/claims")
            .unwrap(),
        0
    );
}

#[test]
fn expired_consolidation_lease_fences_old_owner_and_recovers_once() {
    let path = temp_db_path("consolidation-expired-lease.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open storage");
    storage
        .insert_working(&wm(
            "expired-claim",
            "recoverable working memory",
            "/project/recovery",
        ))
        .unwrap();

    let (old_claims, _) = storage
        .claim_working_lease("sess-1", "/project/recovery", "old-owner", 20, 10)
        .unwrap();
    assert_eq!(old_claims.len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(40));

    let (new_claims, _) = storage
        .claim_working_lease("sess-1", "/project/recovery", "new-owner", 60_000, 10)
        .unwrap();
    assert_eq!(new_claims.len(), 1);
    assert_eq!(new_claims[0].id, old_claims[0].id);
    assert!(
        !storage
            .heartbeat_working_lease(&[old_claims[0].id.clone()], "old-owner", 60_000)
            .unwrap(),
        "expired owner must not renew after takeover"
    );

    let make_summary = |id: &str, content: &str| EpisodicMemoryRecord {
        id: id.into(),
        content: content.into(),
        summary_of: serde_json::to_string(&["expired-claim"]).unwrap(),
        session_id: "sess-1".into(),
        project_scope: "/project/recovery".into(),
        importance: 0.5,
        tier: 1,
        memory_type: "summary".into(),
        metadata: None,
        created_at: "2026-08-21T00:00:00Z".into(),
        updated_at: "2026-08-21T00:00:00Z".into(),
        degraded_at: None,
        valid_until: None,
        superseded_by: None,
    };
    assert!(
        !storage
            .commit_working_lease(
                &old_claims,
                "old-owner",
                &make_summary("stale-summary", "stale summary"),
            )
            .unwrap(),
        "stale owner must not commit after another worker takes the lease"
    );
    assert_eq!(storage.count_episodic().unwrap(), 0);
    assert!(storage
        .commit_working_lease(
            &new_claims,
            "new-owner",
            &make_summary("recovered-summary", "recovered summary"),
        )
        .unwrap());
    assert_eq!(storage.count_episodic().unwrap(), 1);
    assert_eq!(
        storage
            .count_unconsolidated("sess-1", "/project/recovery")
            .unwrap(),
        0
    );
}

#[test]
fn consolidation_dirty_is_project_wide_across_sessions() {
    let path = temp_db_path("consolidation-dirty-project-wide.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open storage");
    let mut other_session = wm(
        "session-b-pending",
        "another session still needs consolidation",
        "/project/shared",
    );
    other_session.session_id = "session-b".into();
    storage.insert_working(&other_session).unwrap();
    storage.mark_consolidation_dirty().unwrap();

    storage
        .clear_consolidation_dirty_if_idle("/project/shared")
        .unwrap();
    assert!(
        storage.is_consolidation_dirty().unwrap(),
        "one session finishing must not clear a project dirty flag while another has pending work"
    );

    storage
        .mark_working_consolidated(&["session-b-pending".into()], "2026-11-22T00:00:00Z")
        .unwrap();
    storage
        .clear_consolidation_dirty_if_idle("/project/shared")
        .unwrap();
    assert!(!storage.is_consolidation_dirty().unwrap());
}

#[test]
fn pending_turn_outbox_is_idempotent_and_recovers_expired_claims() {
    let path = temp_db_path("pending-turn-outbox.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open storage");

    assert!(storage
        .enqueue_pending_turn("session", 1, "/project", "prompt\nanswer", false)
        .unwrap());
    assert!(!storage
        .enqueue_pending_turn("session", 1, "/project", "changed duplicate", true)
        .unwrap());
    let first = storage
        .claim_pending_turn("session", "/project", "worker-a", 60_000)
        .unwrap()
        .expect("first worker claims turn");
    assert_eq!(first.text, "prompt\nanswer");
    assert!(storage
        .claim_pending_turn("session", "/project", "worker-b", 60_000)
        .unwrap()
        .is_none());
    assert!(!storage
        .heartbeat_pending_turn("session", 1, "worker-b", 60_000)
        .unwrap());
    assert!(storage
        .heartbeat_pending_turn("session", 1, "worker-a", 60_000)
        .unwrap());
    assert!(!storage
        .complete_pending_turn("session", 1, "worker-b")
        .unwrap());
    assert!(storage
        .complete_pending_turn("session", 1, "worker-a")
        .unwrap());

    assert!(storage
        .enqueue_pending_turn("session", 2, "/project", "recover after crash", false)
        .unwrap());
    storage
        .claim_pending_turn("session", "/project", "crashed-worker", 1)
        .unwrap()
        .expect("claim before simulated crash");
    std::thread::sleep(std::time::Duration::from_millis(10));
    let recovered = storage
        .claim_pending_turn("session", "/project", "recovery-worker", 60_000)
        .unwrap()
        .expect("expired lease can be reclaimed");
    assert_eq!(recovered.turn_index, 2);
    assert_eq!(recovered.text, "recover after crash");
    assert!(storage
        .complete_pending_turn("session", 2, "recovery-worker")
        .unwrap());
    assert!(storage
        .list_pending_turns("session", 20)
        .unwrap()
        .is_empty());

    drop(storage);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn pending_turn_outbox_survives_storage_reopen_and_reclaims_expired_claim() {
    let path = temp_db_path("pending-turn-restart.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open initial storage");
    assert!(storage
        .enqueue_pending_turn("session", 7, "/project", "durable pending turn", true)
        .unwrap());
    storage
        .claim_pending_turn("session", "/project", "crashed-process", 1)
        .unwrap()
        .expect("claim before simulated process exit");
    drop(storage);

    std::thread::sleep(std::time::Duration::from_millis(10));
    let restarted = TursoStorage::open(&path).expect("reopen storage after process exit");
    let visible = restarted
        .list_pending_turns("session", 20)
        .expect("list durable pending turn");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0]["turnIndex"], 7);
    assert_eq!(visible[0]["status"], "processing");

    let recovered = restarted
        .claim_pending_turn("session", "/project", "restarted-process", 60_000)
        .unwrap()
        .expect("restarted process reclaims expired lease");
    assert_eq!(recovered.turn_index, 7);
    assert_eq!(recovered.text, "durable pending turn");
    assert!(restarted
        .complete_pending_turn("session", 7, "restarted-process")
        .unwrap());
    assert!(restarted
        .list_pending_turns("session", 20)
        .unwrap()
        .is_empty());

    drop(restarted);
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
    let updated = storage
        .get_embedding(MemoryKind::Working, "wm-1")
        .unwrap()
        .unwrap();
    assert_eq!(updated.embedding, vec![0.9]);

    // 不存在的返回 None
    assert!(storage
        .get_embedding(MemoryKind::Episodic, "nope")
        .unwrap()
        .is_none());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn purge_expired_is_scope_safe_and_vector_rehydrate_matches_database() {
    let path = temp_db_path("purge-expired-scope-and-index.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open purge test database");
    let now = "2026-11-23 12:00:00";
    let active_model = pi_memory_core::embedding_runtime::active_model_id();
    let dimension = pi_memory_core::model_manifest::configured_dimension().unwrap();
    let vector = vec![0.01; dimension];

    let mut expired_by_time = wm("wm-expired-time", "expired target working", "/target");
    expired_by_time.valid_until = Some("2020-01-01 00:00:00".into());
    let mut superseded = wm("wm-superseded", "superseded target working", "/target");
    superseded.superseded_by = Some("wm-replacement".into());
    let active_target = wm("wm-active-target", "active target working", "/target");
    let expired_other = wm("wm-expired-other", "expired other working", "/other");
    let mut expired_other = expired_other;
    expired_other.valid_until = Some("2020-01-01 00:00:00".into());
    for record in [expired_by_time, superseded, active_target, expired_other] {
        storage.insert_working(&record).unwrap();
        storage
            .save_embedding(&MemoryEmbeddingRecord {
                memory_kind: MemoryKind::Working,
                memory_id: record.id,
                embedding: vector.clone(),
                model: active_model.clone(),
            })
            .unwrap();
    }

    let episodic = |id: &str, scope: &str, valid_until: Option<&str>| EpisodicMemoryRecord {
        id: id.into(),
        content: format!("episodic content {id}"),
        summary_of: "[]".into(),
        session_id: "session".into(),
        project_scope: scope.into(),
        importance: 0.5,
        tier: 1,
        memory_type: "summary".into(),
        metadata: None,
        created_at: "2026-01-01 00:00:00".into(),
        updated_at: "2026-01-01 00:00:00".into(),
        degraded_at: None,
        valid_until: valid_until.map(str::to_string),
        superseded_by: None,
    };
    for record in [
        episodic("ep-expired-target", "/target", Some("2020-01-01 00:00:00")),
        episodic("ep-active-target", "/target", None),
        episodic("ep-expired-other", "/other", Some("2020-01-01 00:00:00")),
    ] {
        storage.insert_episodic(&record).unwrap();
        storage
            .save_embedding(&MemoryEmbeddingRecord {
                memory_kind: MemoryKind::Episodic,
                memory_id: record.id,
                embedding: vector.clone(),
                model: active_model.clone(),
            })
            .unwrap();
    }
    storage
        .save_embedding(&MemoryEmbeddingRecord {
            memory_kind: MemoryKind::Working,
            memory_id: "missing-memory".into(),
            embedding: vector,
            model: active_model,
        })
        .unwrap();

    let mut vectors = ProjectVectors::new().unwrap();
    vectors.hydrate(&storage).unwrap();
    assert_eq!(vectors.index_len(), 7);

    assert_eq!(storage.purge_expired("/target", now).unwrap(), 3);
    assert!(storage.get_working("wm-expired-time").unwrap().is_none());
    assert!(storage.get_working("wm-superseded").unwrap().is_none());
    assert!(storage.get_working("wm-active-target").unwrap().is_some());
    assert!(storage.get_working("wm-expired-other").unwrap().is_some());
    assert!(storage.get_episodic("ep-expired-target").unwrap().is_none());
    assert!(storage.get_episodic("ep-expired-other").unwrap().is_some());
    assert!(storage
        .get_embedding(MemoryKind::Working, "wm-expired-time")
        .unwrap()
        .is_none());
    assert!(storage
        .get_embedding(MemoryKind::Episodic, "ep-expired-target")
        .unwrap()
        .is_none());

    assert_eq!(storage.purge_orphan_embeddings().unwrap(), 1);
    vectors.hydrate(&storage).unwrap();
    assert_eq!(vectors.index_len(), 4);
    drop(vectors);
    drop(storage);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{path}{suffix}"));
    }
}

#[cfg(unix)]
#[test]
fn project_database_files_and_directories_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "pi-memory-permissions-{}-{nonce}",
        std::process::id()
    ));
    let db_path = pi_memory_core::paths::ensure_project_db_dir(&base, "/private/project").unwrap();
    let storage = TursoStorage::open_deferred(db_path.to_str().unwrap()).unwrap();
    storage
        .insert_working(&wm(
            "private-memory",
            "secret memory body",
            "/private/project",
        ))
        .unwrap();

    let project_dir = db_path.parent().unwrap();
    for directory in [base.as_path(), base.join("memories").as_path(), project_dir] {
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "private directory permissions: {}",
            directory.display()
        );
    }
    assert_eq!(
        std::fs::metadata(&db_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(storage);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn turso_tantivy_fts_match_search() {
    let path = temp_db_path("turso-fts.db");
    let _ = std::fs::remove_file(&path);

    let storage = TursoStorage::open(&path).expect("打开失败");
    storage
        .insert_working(&wm("wm-fts-1", "项目使用 turbovec 向量索引", "/tmp/demo"))
        .unwrap();
    storage
        .insert_working(&wm("wm-fts-2", "另一条无关记忆", "/tmp/demo"))
        .unwrap();

    // 新 search_text 索引按 CJK bigram 物化词项，短子串也应命中。
    let hits = storage
        .search_working_fts("向量索引", "/tmp/demo")
        .expect("FTS 搜索失败");
    assert!(!hits.is_empty());
    assert_eq!(hits[0].0.id, "wm-fts-1");

    let partial = storage
        .search_working_fts("向量", "/tmp/demo")
        .expect("FTS 搜索失败");
    assert_eq!(
        partial.first().map(|hit| hit.0.id.as_str()),
        Some("wm-fts-1")
    );

    // scope 过滤
    let other_scope = storage
        .search_working_fts("向量索引", "/other")
        .expect("FTS 搜索失败");
    assert!(other_scope.is_empty(), "其他 scope 不应命中");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn fts_v2_invariant_recovers_when_index_is_missing() {
    let path = temp_db_path("turso-fts-v2-recovery.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("open");
    storage
        .insert_working(&wm(
            "wm-recovery",
            "项目使用 turbovec 向量索引 keyword",
            "/p/recovery",
        ))
        .expect("insert");
    let before = storage.diagnose().expect("diagnose before");
    assert_eq!(before["fts_state"], "search_text");
    assert_eq!(before["fts_invariant_ok"], true);

    // Model a crash/external index loss after v2 became active. On the next open,
    // init must restore content-index reads before attempting to rebuild v2.
    storage.drop_fts().expect("drop indexes");
    storage.drop_fts().expect("drop FTS before cleanup");
    drop(storage);
    let recovered = TursoStorage::open_deferred(&path).expect("reopen without migration");
    let diag = recovered.diagnose().expect("diagnose recovered state");
    assert_eq!(diag["fts_state"], "content");
    assert_eq!(diag["fts_dirty"], true);
    assert_eq!(diag["fts_invariant_ok"], true);
    assert_eq!(
        recovered
            .search_working_fts_top_k("keyword", "/p/recovery", 10)
            .expect("content fallback search")
            .first()
            .map(|hit| hit.0.id.as_str()),
        Some("wm-recovery")
    );

    recovered
        .migrate_search_text_if_needed()
        .expect("rebuild and switch to v2");
    let after = recovered.diagnose().expect("diagnose rebuilt state");
    assert_eq!(after["fts_state"], "search_text");
    assert_eq!(after["fts_dirty"], false);
    assert_eq!(after["fts_invariant_ok"], true);
    assert_eq!(
        recovered
            .search_working_fts_top_k("向量", "/p/recovery", 10)
            .expect("CJK search after recovery")
            .first()
            .map(|hit| hit.0.id.as_str()),
        Some("wm-recovery")
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_fts_update_sync() {
    let path = temp_db_path("turso-fts-update.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    // 插入旧内容
    storage.begin_tx().expect("begin update tx");
    storage
        .insert_working(&wm("wm-u", "旧内容没有关键词", "/tmp/demo"))
        .unwrap();
    storage.commit_tx().expect("commit old content");
    assert!(storage
        .search_working_fts("keyword", "/tmp/demo")
        .unwrap()
        .is_empty());

    // upsert 为新内容（含关键词）
    storage.begin_tx().expect("begin replacement tx");
    storage
        .insert_working(&wm("wm-u", "新内容包含 keyword 关键词", "/tmp/demo"))
        .unwrap();
    storage.commit_tx().expect("commit replacement");
    let hits = storage
        .search_working_fts("keyword", "/tmp/demo")
        .expect("FTS 搜索失败");
    assert!(!hits.is_empty(), "Tantivy FTS 应同步 upsert 后的内容");
    assert_eq!(hits[0].0.id, "wm-u");

    drop(storage);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_rebuild_fts() {
    let path = temp_db_path("turso-rebuild.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");

    storage
        .insert_working(&wm("wm-r", "rebuild 后仍可命中 keyword", "/tmp/demo"))
        .unwrap();
    storage.rebuild_fts().expect("rebuild 失败");

    let hits = storage
        .search_working_fts("keyword", "/tmp/demo")
        .expect("FTS 搜索失败");
    assert!(!hits.is_empty(), "rebuild 后应仍能命中");
    assert_eq!(hits[0].0.id, "wm-r");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn turso_diagnose() {
    let path = temp_db_path("turso-diagnose.db");
    let _ = std::fs::remove_file(&path);
    let storage = TursoStorage::open(&path).expect("打开失败");
    storage
        .insert_working(&wm("wm-1", "x", "/tmp/demo"))
        .unwrap();

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
    s1.insert_working(&wm("wm-1", "content a", "/p"))
        .expect("insert 1");

    // s2 在 s1 事务未提交时写入：busy_timeout 内等待，s1 提交后成功
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        s2.insert_working(&wm("wm-2", "content b", "/p"))
            .map(|_| ())
    });
    std::thread::sleep(std::time::Duration::from_millis(600));
    s1.commit_tx().expect("commit");

    let result = t.join().expect("join");
    assert!(
        result.is_ok(),
        "busy_timeout 下并发写应等待成功: {result:?}"
    );
    assert_eq!(
        s1.list_working_by_project("/p").expect("list").len(),
        2,
        "两次写入都应落库"
    );

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
            fact_id: format!("f{i}"),
            subject: format!("用户{i}"),
            predicate: "使用".into(),
            object: format!("特性{i}"),
            confidence: 0.8,
            source_memory_id: None,
            session_id: "s1".into(),
            project_scope: "/p".into(),
            timestamp: "2026-08-21".into(),
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
            id: format!("w{i}"),
            content: format!("alpha 内容 {i:05}"),
            embed_text: None,
            source: Some("t".into()),
            session_id: "s1".into(),
            project_scope: "/p".into(),
            importance: 0.5,
            memory_type: "working".into(),
            metadata: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            consolidated_at: None,
            valid_until: None,
            superseded_by: None,
        })
        .expect("insert working");
    }
    db.commit_tx().expect("commit 2");

    // 数据完整 + FTS 重建后可搜
    assert_eq!(db.list_all_facts().expect("facts").len(), 10_000);
    assert_eq!(
        db.list_working_by_project("/p").expect("working").len(),
        10_000
    );
    db.rebuild_fts().expect("rebuild fts");
    let hits = db.search_working_fts("alpha", "/p").expect("fts");
    assert!(
        hits.len() >= 10_000,
        "FTS 应命中全部（实际 {}）",
        hits.len()
    );
    let top_k = db
        .search_working_fts_top_k("alpha", "/p", 50)
        .expect("fts top-k");
    assert_eq!(top_k.len(), 50, "Top-K 查询必须只返回请求条数");
    let plan = db
        .explain_working_fts_top_k("alpha", 50)
        .expect("explain fts top-k");
    assert!(
        plan.iter()
            .all(|line| !line.contains("USE SORTER FOR ORDER BY")),
        "FTS Top-K should not fall back to a full result sort: {plan:?}"
    );

    let _ = std::fs::remove_file(&path);
}
