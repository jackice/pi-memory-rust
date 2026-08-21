//! LLM 结果集成测试：llmResult 解析、降级、degraded 标志。

use pi_memory_core::embedding_runtime::EmbeddingRuntime;
use pi_memory_core::runtime::{build_recall_injection, consolidate_session_memories, retain_turn_memories};
use pi_memory_core::storage::{Storage, TursoStorage};

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
    let mut embedding = EmbeddingRuntime::new();
    // 模型未就绪时 EmbeddingRuntime::new 不下载（测试环境无模型，走降级）

    let llm_json = r#"{
        "facts": [{"subject": "项目", "predicate": "采用", "object": "Rust"}],
        "preferences": [{"preference": "使用中文", "topic": "language"}],
        "instructions": [{"instruction": "提交前测试", "topic": "workflow"}],
        "timeline": [{"date": "2026-08-21", "description": "M3 完成"}]
    }"#;
    retain_turn_memories(
        &storage, 1, "会话文本", "s1", "/tmp/llm", &mut embedding, Some(llm_json),
    )
    .expect("retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 1);
    assert_eq!(storage.count_by_session("preferences", "s1").unwrap(), 1);
    assert_eq!(storage.count_by_session("instructions", "s1").unwrap(), 1);
    assert_eq!(storage.count_by_session("timeline", "s1").unwrap(), 1);

    let facts = storage.search_facts_like("Rust").unwrap();
    assert_eq!(facts[0].subject, "项目");
    assert_eq!(facts[0].predicate, "采用");
    // 补全字段
    assert_eq!(facts[0].session_id, "s1");
    assert!(facts[0].source_memory_id.is_some());
}

#[test]
fn retain_invalid_llm_result_falls_back_to_rules() {
    let storage = TursoStorage::open(&temp_db("llm-fallback.db")).expect("打开失败");
    let mut embedding = EmbeddingRuntime::new();

    // 无效 JSON → 降级规则路径（文本含「事实:」前缀）
    retain_turn_memories(
        &storage, 1, "事实: 首版正式采用本地 SQLite", "s1", "/tmp/llm", &mut embedding, Some("not json at all"),
    )
    .expect("retain 失败");

    assert_eq!(storage.count_by_session("facts", "s1").unwrap(), 1, "应降级规则提取");
    let facts = storage.search_facts_like("SQLite").unwrap();
    assert_eq!(facts[0].subject, "首版");
    assert_eq!(facts[0].predicate, "正式采用");
}

#[test]
fn consolidate_uses_llm_summary_and_degraded_flag() {
    let storage = TursoStorage::open(&temp_db("llm-consolidate.db")).expect("打开失败");
    let mut embedding = EmbeddingRuntime::new();

    retain_turn_memories(&storage, 1, "记忆 A", "s1", "/tmp/llm", &mut embedding, None).unwrap();
    retain_turn_memories(&storage, 2, "记忆 B", "s1", "/tmp/llm", &mut embedding, None).unwrap();

    // LLM 成功：content 用摘要，degraded=false
    let ok = consolidate_session_memories(&storage, "s1", "/tmp/llm", Some("LLM 生成的会话摘要"), false).unwrap();
    assert!(!ok.degraded);
    assert_eq!(ok.episodic_count, 1);
    let episodics = storage.list_episodic_by_project("/tmp/llm").unwrap();
    assert_eq!(episodics[0].content, "LLM 生成的会话摘要");

    // 再 retain 一轮（consolidated 标记后新记忆）验证 LLM 失败 degraded
    retain_turn_memories(&storage, 3, "记忆 C", "s1", "/tmp/llm", &mut embedding, None).unwrap();
    let failed = consolidate_session_memories(&storage, "s1", "/tmp/llm", None, true).unwrap();
    assert!(failed.degraded, "LLM 失败应标记 degraded");

    // 无 LLM（未配置）：degraded=false，规则拼接
    retain_turn_memories(&storage, 4, "记忆 D", "s1", "/tmp/llm", &mut embedding, None).unwrap();
    let none = consolidate_session_memories(&storage, "s1", "/tmp/llm", None, false).unwrap();
    assert!(!none.degraded);
    let episodics = storage.list_episodic_by_project("/tmp/llm").unwrap();
    assert!(episodics[1].content.contains("记忆 C"), "无 LLM 时规则拼接内容");
}

#[test]
fn recall_with_llm_extracted_structured() {
    let storage = TursoStorage::open(&temp_db("llm-recall.db")).expect("打开失败");
    let mut embedding = EmbeddingRuntime::new();

    let llm_json = r#"{"preferences": [{"preference": "回复使用中文"}]}"#;
    retain_turn_memories(&storage, 1, "正文", "s1", "/tmp/llm", &mut embedding, Some(llm_json)).unwrap();

    // 结构化候选应被 recall 命中
    let injection = build_recall_injection(&storage, "中文", "/tmp/llm", "s1", &mut embedding).unwrap();
    assert!(injection.contains("回复使用中文"), "LLM 提取的偏好应被召回: {injection}");
}
