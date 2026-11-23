//! LLM 结果集成测试：llmResult 解析、降级、degraded 标志。

use pi_memory_core::embedding_runtime::{ProjectVectors, SharedEmbedder};
use pi_memory_core::models::{
    EpisodicMemoryRecord, FactRecord, InstructionRecord, WorkingMemoryRecord,
};
use pi_memory_core::runtime::{
    build_recall_injection, claim_consolidation, commit_consolidation,
    consolidate_session_memories, retain_turn_memories,
};
use pi_memory_core::storage::TursoStorage;

fn temp_db(name: &str) -> String {
    let dir = std::env::temp_dir().join("pi-memory-rust-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name).to_string_lossy().into_owned();
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn retain_parses_llm_result_and_upserts_structured() {
    let storage = TursoStorage::open(&temp_db("llm-retain.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();
    // 模型未就绪时 EmbeddingRuntime::new 不下载（测试环境无模型，走降级）

    let llm_json = r#"{
        "facts": [{"subject": "项目", "predicate": "采用", "object": "Rust", "timestamp": "forged"}],
        "preferences": [{"preference": "使用中文", "topic": "language", "createdAt": "forged"}],
        "instructions": [{"instruction": "提交前测试", "topic": "workflow", "createdAt": "forged"}],
        "timeline": [{"date": "2026-08-21", "description": "M3 完成", "createdAt": "forged", "source": "forged"}]
    }"#;
    retain_turn_memories(
        &storage,
        1,
        "会话文本",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        Some(llm_json),
    )
    .expect("retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 1);
    assert_eq!(storage.count_by_session("preferences", "s1").unwrap(), 1);
    assert_eq!(storage.count_by_session("instructions", "s1").unwrap(), 1);
    assert_eq!(storage.count_by_session("timeline", "s1").unwrap(), 1);
    assert_eq!(
        storage.list_all_timeline().unwrap()[0].source.as_deref(),
        Some("llm")
    );
    assert_eq!(
        storage.list_all_timeline().unwrap()[0].created_at,
        storage.list_all_working().unwrap()[0].created_at
    );

    let facts = storage.search_facts_like("Rust", "/tmp/llm").unwrap();
    assert_eq!(facts[0].subject, "项目");
    assert_eq!(facts[0].predicate, "采用");
    assert_eq!(
        facts[0].timestamp,
        storage.list_all_working().unwrap()[0].created_at
    );
    assert_eq!(
        storage
            .search_preferences_like("使用中文", "/tmp/llm")
            .unwrap()[0]
            .created_at,
        storage.list_all_working().unwrap()[0].created_at
    );
    assert_eq!(
        storage
            .search_instructions_like("提交前测试", "/tmp/llm")
            .unwrap()[0]
            .created_at,
        storage.list_all_working().unwrap()[0].created_at
    );
    // 补全字段
    assert_eq!(facts[0].session_id, "s1");
    assert!(facts[0].source_memory_id.is_some());
}

#[test]
fn retain_repairs_legacy_partial_turn() {
    let storage = TursoStorage::open(&temp_db("llm-retain-legacy-partial.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();
    let text = "事实:项目采用Rust。";
    let memory_id = "legacy-session:turn:1";

    storage
        .insert_working(&WorkingMemoryRecord {
            id: memory_id.into(),
            content: text.into(),
            embed_text: Some(text.into()),
            source: Some("turn_end".into()),
            session_id: "legacy-session".into(),
            project_scope: "/tmp/legacy".into(),
            importance: 0.5,
            memory_type: "working".into(),
            metadata: Some(serde_json::json!({
                "retain_state": "partial",
                "source_sha": "stale-source-hash"
            })),
            created_at: "2026-01-01T00:00:00.000Z".into(),
            updated_at: "2026-01-01T00:00:00.000Z".into(),
            consolidated_at: None,
            valid_until: None,
            superseded_by: None,
        })
        .unwrap();
    storage
        .insert_facts(&[FactRecord {
            fact_id: format!("{memory_id}:fact:llm-only"),
            subject: "仅由旧模型得到".into(),
            predicate: "不可重放".into(),
            object: "旧结果".into(),
            confidence: 0.99,
            source_memory_id: Some(memory_id.into()),
            session_id: "legacy-session".into(),
            project_scope: "/tmp/legacy".into(),
            timestamp: "2026-01-01T00:00:00.000Z".into(),
        }])
        .unwrap();

    retain_turn_memories(
        &storage,
        1,
        text,
        "legacy-session",
        "/tmp/legacy",
        &shared,
        &mut vectors,
        None,
    )
    .expect("legacy partial retain 应可重放");

    let facts = storage.list_all_facts().unwrap();
    assert_eq!(
        facts.len(),
        1,
        "旧结构化项应先清理再从 working.content 重放"
    );
    assert_eq!(facts[0].subject, "项目");
    assert_eq!(facts[0].predicate, "采用");
    assert_eq!(facts[0].object, "Rust");
    assert_eq!(facts[0].source_memory_id.as_deref(), Some(memory_id));

    let repaired = storage.get_working(memory_id).unwrap().unwrap();
    let metadata = repaired.metadata.unwrap();
    assert_eq!(metadata["retain"], "rule_complete");
    assert_eq!(metadata["extractor"], "rule");
    assert!(metadata.get("retain_state").is_none());
}

#[test]
fn retain_persists_multiple_structured_memories_without_id_collisions() {
    let storage = TursoStorage::open(&temp_db("llm-multiple-structured.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();
    let llm_json = r#"{
        "facts": [
            {"factId": "forged", "subject": "项目", "predicate": "采用", "object": "Rust"},
            {"factId": "forged", "subject": "项目", "predicate": "采用", "object": "Rust"},
            {"factId": "forged", "subject": " 项目 ", "predicate": "采用", "object": " Rust "},
            {"factId": "forged", "subject": "部署", "predicate": "运行于", "object": "Linux"},
            {"subject": "测试", "predicate": "覆盖", "object": "集成"}
        ],
        "preferences": [
            {"id": "forged", "preference": "使用中文", "topic": "language"},
            {"id": "forged", "preference": "先写测试", "topic": "workflow"},
            {"id": "forged", "preference": "保留兼容性", "topic": "design"}
        ],
        "instructions": [
            {"id": "forged", "instruction": "提交前测试", "topic": "workflow"},
            {"id": "forged", "instruction": "保留兼容性", "topic": "design"},
            {"id": "forged", "instruction": "先确认写入范围", "topic": "safety"}
        ],
        "timeline": [
            {"eventId": "forged", "date": "2026-08-21", "description": "M3 完成"},
            {"eventId": "forged", "date": "2026-08-22", "description": "发布候选版"},
            {"eventId": "forged", "date": "2026-08-23", "description": "完成兼容性验证"}
        ]
    }"#;

    retain_turn_memories(
        &storage,
        1,
        "多条结构化记忆",
        "s1",
        "/tmp/llm-multiple",
        &shared,
        &mut vectors,
        Some(llm_json),
    )
    .expect("retain 失败");

    retain_turn_memories(
        &storage,
        1,
        "多条结构化记忆",
        "s1",
        "/tmp/llm-multiple",
        &shared,
        &mut vectors,
        Some(llm_json),
    )
    .expect("重复 retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 3);
    assert_eq!(storage.count_by_session("preferences", "s1").unwrap(), 3);
    assert_eq!(storage.count_by_session("instructions", "s1").unwrap(), 3);
    assert_eq!(storage.count_by_session("timeline", "s1").unwrap(), 3);

    let facts = storage.list_all_facts().unwrap();
    let preferences = storage.list_all_preferences().unwrap();
    let instructions = storage.list_all_instructions().unwrap();
    let timeline = storage.list_all_timeline().unwrap();
    assert_eq!(facts.len(), 3);
    assert!(facts.iter().all(|item| item.fact_id != "forged"));
    assert!(preferences.iter().all(|item| item.id != "forged"));
    assert!(instructions.iter().all(|item| item.id != "forged"));
    assert!(timeline.iter().all(|item| item.event_id != "forged"));
}

#[test]
fn retain_filters_empty_llm_items_and_falls_back_to_rules() {
    let storage = TursoStorage::open(&temp_db("llm-empty-structured.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();

    retain_turn_memories(
        &storage,
        1,
        "事实: 首版正式采用本地 SQLite",
        "s1",
        "/tmp/llm-empty",
        &shared,
        &mut vectors,
        Some(r#"{"facts":[{"subject":" ","predicate":"采用","object":"SQLite"}]}"#),
    )
    .expect("retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 1);
    assert_eq!(
        storage
            .search_facts_like("SQLite", "/tmp/llm-empty")
            .unwrap()[0]
            .subject,
        "首版"
    );
}

#[test]
fn retain_limits_structured_items_to_32_and_trims_stored_fields() {
    let storage = TursoStorage::open(&temp_db("llm-structured-limit.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();
    let mut facts = (0..34)
        .map(|index| {
            serde_json::json!({
                "subject": format!(" subject {index} "),
                "predicate": " uses ",
                "object": format!(" object {index} ")
            })
        })
        .collect::<Vec<_>>();
    facts.insert(
        0,
        serde_json::json!({"subject":" ","predicate":"uses","object":"empty"}),
    );
    facts.insert(
        1,
        serde_json::json!({"subject":"missing object","predicate":"uses","object":" "}),
    );
    let payload = serde_json::json!({"facts": facts}).to_string();

    retain_turn_memories(
        &storage,
        1,
        "many facts",
        "s1",
        "/tmp/llm-limit",
        &shared,
        &mut vectors,
        Some(&payload),
    )
    .expect("retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 32);
    let hits = storage
        .search_facts_like("object 0", "/tmp/llm-limit")
        .unwrap();
    assert_eq!(hits[0].subject, "subject 0");
    assert_eq!(hits[0].predicate, "uses");
    assert_eq!(hits[0].object, "object 0");
}

#[test]
fn retain_deduplicates_before_applying_the_per_kind_limit() {
    let storage = TursoStorage::open(&temp_db("llm-dedup-before-limit.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();
    let mut facts = vec![
        serde_json::json!({
            "subject": "same", "predicate": "uses", "object": "duplicate"
        });
        40
    ];
    facts.extend((0..31).map(|index| {
        serde_json::json!({
            "subject": "unique", "predicate": "uses", "object": format!("item-{index}")
        })
    }));
    let payload = serde_json::json!({"facts": facts}).to_string();

    retain_turn_memories(
        &storage,
        1,
        "duplicate items before distinct items",
        "s1",
        "/tmp/llm-dedup-limit",
        &shared,
        &mut vectors,
        Some(&payload),
    )
    .expect("retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 32);
    let retained = storage.list_all_facts().unwrap();
    assert!(
        retained.iter().any(|fact| fact.object == "item-30"),
        "前面的重复项不能挤占后续唯一内容的名额"
    );
}

#[test]
fn retain_invalid_llm_result_falls_back_to_rules() {
    let storage = TursoStorage::open(&temp_db("llm-fallback.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();

    // 无效 JSON → 降级规则路径（文本含「事实:」前缀）
    retain_turn_memories(
        &storage,
        1,
        "事实: 首版正式采用本地 SQLite\n时间线: 2026-08-19 完成 Task 2 的 SQLite schema",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        Some("not json at all"),
    )
    .expect("retain 失败");

    assert_eq!(
        storage.count_by_session("facts", "s1").unwrap(),
        1,
        "应降级规则提取"
    );
    let facts = storage.search_facts_like("SQLite", "/tmp/llm").unwrap();
    assert_eq!(facts[0].subject, "首版");
    assert_eq!(facts[0].predicate, "正式采用");
    assert_eq!(
        facts[0].fact_id,
        pi_memory_core::ids::stable_id("s1:turn:1", "fact", &["首版", "正式采用", "本地 SQLite"])
    );
    assert_eq!(
        storage.list_all_timeline().unwrap()[0].source.as_deref(),
        Some("retained-turn")
    );
}

#[test]
fn consolidate_uses_llm_summary_and_degraded_flag() {
    let storage = TursoStorage::open(&temp_db("llm-consolidate.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();

    retain_turn_memories(
        &storage,
        1,
        "记忆 A",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        None,
    )
    .unwrap();
    retain_turn_memories(
        &storage,
        2,
        "记忆 B",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        None,
    )
    .unwrap();

    // LLM 成功：content 用摘要，degraded=false
    let ok = consolidate_session_memories(
        &storage,
        "s1",
        "/tmp/llm",
        Some("LLM 生成的会话摘要"),
        false,
    )
    .unwrap();
    assert!(!ok.degraded);
    assert_eq!(ok.episodic_count, 1);
    let episodics = storage.list_episodic_by_project("/tmp/llm").unwrap();
    assert_eq!(episodics[0].content, "LLM 生成的会话摘要");

    // 再 retain 一轮（consolidated 标记后新记忆）验证 LLM 失败 degraded
    retain_turn_memories(
        &storage,
        3,
        "记忆 C",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        None,
    )
    .unwrap();
    let failed = consolidate_session_memories(&storage, "s1", "/tmp/llm", None, true).unwrap();
    assert!(failed.degraded, "LLM 失败应标记 degraded");

    // 无 LLM（未配置）：degraded=false，规则拼接
    retain_turn_memories(
        &storage,
        4,
        "记忆 D",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        None,
    )
    .unwrap();
    let none = consolidate_session_memories(&storage, "s1", "/tmp/llm", None, false).unwrap();
    assert!(!none.degraded);
    let episodics = storage.list_episodic_by_project("/tmp/llm").unwrap();
    assert!(
        episodics[1].content.contains("记忆 C"),
        "无 LLM 时规则拼接内容"
    );
}

#[test]
fn updating_consolidated_working_invalidates_and_reopens_the_whole_summary_set() {
    let path = temp_db("llm-consolidated-working-update.db");
    let storage = TursoStorage::open(&path).expect("open storage");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();

    retain_turn_memories(
        &storage,
        1,
        "首轮记录旧内容",
        "session-update",
        "/tmp/update",
        &shared,
        &mut vectors,
        None,
    )
    .unwrap();
    retain_turn_memories(
        &storage,
        2,
        "第二轮保持原样",
        "session-update",
        "/tmp/update",
        &shared,
        &mut vectors,
        None,
    )
    .unwrap();

    let old_claim = claim_consolidation(
        &storage,
        "session-update",
        "/tmp/update",
        "old-owner",
        60_000,
        10,
    )
    .unwrap();
    assert_eq!(old_claim.working.len(), 2);
    let summary_ids = old_claim
        .working
        .iter()
        .map(|claim| claim.id.clone())
        .collect::<Vec<_>>();
    let summary_of = serde_json::to_string(&summary_ids).unwrap();
    storage
        .mark_working_consolidated(&summary_ids, "2026-11-21T00:00:00Z")
        .unwrap();
    storage
        .insert_episodic(&EpisodicMemoryRecord {
            id: "old-episodic".into(),
            content: "基于旧 working 的摘要".into(),
            summary_of,
            session_id: "session-update".into(),
            project_scope: "/tmp/update".into(),
            importance: 0.7,
            tier: 1,
            memory_type: "summary".into(),
            metadata: None,
            created_at: "2026-11-21T00:00:00Z".into(),
            updated_at: "2026-11-21T00:00:00Z".into(),
            degraded_at: None,
            valid_until: None,
            superseded_by: None,
        })
        .unwrap();

    retain_turn_memories(
        &storage,
        1,
        "首轮记录已更正的新内容",
        "session-update",
        "/tmp/update",
        &shared,
        &mut vectors,
        Some(r#"{"facts":[{"subject":"首轮","predicate":"记录","object":"已更正"}]}"#),
    )
    .unwrap();

    let old_summary = storage
        .list_episodic_by_project("/tmp/update")
        .unwrap()
        .into_iter()
        .find(|row| row.id == "old-episodic")
        .unwrap();
    assert!(old_summary.valid_until.is_some());
    assert!(storage.is_consolidation_dirty().unwrap());
    let reopened = storage.list_working_by_project("/tmp/update").unwrap();
    assert_eq!(reopened.len(), 2);
    assert!(reopened.iter().all(|row| row.consolidated_at.is_none()));

    let new_claim = claim_consolidation(
        &storage,
        "session-update",
        "/tmp/update",
        "new-owner",
        60_000,
        10,
    )
    .unwrap();
    assert_eq!(new_claim.working.len(), 2);
    assert!(new_claim
        .working
        .iter()
        .any(|claim| claim.content == "首轮记录已更正的新内容"));
    let committed = commit_consolidation(
        &storage,
        "new-owner",
        Some("包含更正后内容的新摘要"),
        false,
        &shared,
        &mut vectors,
    )
    .unwrap();
    assert_eq!(committed["committed"], true);
    assert!(!storage.is_consolidation_dirty().unwrap());

    let episodic = storage.list_episodic_by_project("/tmp/update").unwrap();
    let new_summary = episodic
        .iter()
        .find(|row| row.id != "old-episodic")
        .unwrap();
    assert_eq!(new_summary.content, "包含更正后内容的新摘要");
    assert!(new_summary.valid_until.is_none());
    let new_summary_ids: Vec<String> = serde_json::from_str(&new_summary.summary_of).unwrap();
    assert_eq!(new_summary_ids.len(), 2);
    assert_eq!(
        new_summary_ids
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        summary_ids.into_iter().collect()
    );
}

#[test]
fn recall_with_llm_extracted_structured() {
    let storage = TursoStorage::open(&temp_db("llm-recall.db")).expect("打开失败");
    let shared = SharedEmbedder::new();
    let mut vectors = ProjectVectors::new().unwrap();

    let llm_json = r#"{"facts": [{"subject": "回复", "predicate": "使用", "object": "中文"}]}"#;
    retain_turn_memories(
        &storage,
        1,
        "正文",
        "s1",
        "/tmp/llm",
        &shared,
        &mut vectors,
        Some(llm_json),
    )
    .unwrap();
    storage
        .insert_instructions(&[InstructionRecord {
            id: "inactive-instruction".into(),
            instruction: "回复使用中文".into(),
            topic: Some("language".into()),
            active: false,
            context_snippet: None,
            source_memory_id: None,
            session_id: "s1".into(),
            project_scope: "/tmp/llm".into(),
            created_at: "2026-11-22T00:00:00Z".into(),
        }])
        .unwrap();

    // 结构化候选应被 recall 命中
    let injection = build_recall_injection(
        &storage,
        "怎样避免多个智能体同时修改代码冲突，回复使用中文",
        "/tmp/llm",
        "s1",
        &shared,
        &mut vectors,
    )
    .unwrap();
    assert!(
        injection.contains("回复使用中文"),
        "长 prompt 中的事实子句应按词项召回: {injection}"
    );
    assert!(
        injection.contains("[fact]"),
        "structured fact 应被召回: {injection}"
    );
    assert!(
        !injection.contains("[instruction]"),
        "inactive instruction 不得被召回: {injection}"
    );
}
