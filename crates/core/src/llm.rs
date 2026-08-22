//! LLM 提示模板与结果解析（对应 TS 版 prompts/ + fromJsonPayload）。
//!
//! LLM 输出由 TS 侧生成后作为 `llmResult` 传入（保持单向 RPC），
//! Rust 负责解析与降级：解析失败 → 规则路径。

use crate::models::{FactRecord, InstructionRecord, PreferenceRecord, TimelineEventRecord};

/// 结构化提取提示（对应 extract-structured.md，明确 JSON 输出格式）。
pub const EXTRACT_PROMPT: &str = r#"请从下方会话片段中抽取 facts、preferences、instructions、timeline 四类结构化记忆，并**只返回 JSON**（不要任何其他文字、解释或 markdown）：
{
  "facts": [{"subject": "主语", "predicate": "谓语", "object": "宾语"}],
  "preferences": [{"preference": "偏好描述", "topic": "主题"}],
  "instructions": [{"instruction": "指令内容", "topic": "主题"}],
  "timeline": [{"date": "YYYY-MM-DD", "description": "事件描述"}]
}
规则：无法可靠抽取的类别返回空数组；不要编造；只输出 JSON 本身。

会话片段："#;

/// 会话整合提示（对应 consolidate-session.md）。
pub const CONSOLIDATE_PROMPT: &str = r#"请把本次 session 的 working memory 总结为 1 条可复用的 episodic memory；若信息不足，请返回简洁摘要，不要阻塞流程。"#;

/// 解析 LLM 返回的结构化 JSON（对应 fromJsonPayload）。
/// 返回 None 表示解析失败（调用方降级规则路径）。
#[derive(Debug, Clone, Default)]
pub struct LlmExtractionResult {
    pub facts: Vec<FactRecord>,
    pub preferences: Vec<PreferenceRecord>,
    pub instructions: Vec<InstructionRecord>,
    pub timeline: Vec<TimelineEventRecord>,
}

/// 从 LLM JSON 输出解析结构化记忆；解析失败返回默认空结果。
/// 注意：LLM 返回的 id/sessionId 等字段由调用方补齐（LLM 只给内容字段）。
pub fn parse_llm_extraction(payload: &str) -> LlmExtractionResult {
    let parsed: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return LlmExtractionResult::default(),
    };
    LlmExtractionResult {
        facts: parse_list(&parsed, "facts", |v| FactRecord {
            fact_id: v.get("factId").or(v.get("fact_id")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
            subject: v.get("subject").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            predicate: v.get("predicate").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            object: v.get("object").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            confidence: v.get("confidence").and_then(|x| x.as_f64()).unwrap_or(0.5),
            source_memory_id: None,
            session_id: String::new(),
            timestamp: String::new(),
        }),
        preferences: parse_list(&parsed, "preferences", |v| PreferenceRecord {
            id: String::new(),
            preference: v.get("preference").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            topic: v.get("topic").and_then(|x| x.as_str()).map(String::from),
            context_snippet: None,
            source_memory_id: None,
            session_id: String::new(),
            created_at: String::new(),
        }),
        instructions: parse_list(&parsed, "instructions", |v| InstructionRecord {
            id: String::new(),
            instruction: v.get("instruction").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            topic: v.get("topic").and_then(|x| x.as_str()).map(String::from),
            active: v.get("active").and_then(|x| x.as_bool()).unwrap_or(true),
            context_snippet: None,
            source_memory_id: None,
            session_id: String::new(),
            created_at: String::new(),
        }),
        timeline: parse_list(&parsed, "timeline", |v| TimelineEventRecord {
            event_id: String::new(),
            date: v.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            description: v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            source: None,
            source_memory_id: None,
            session_id: String::new(),
            created_at: String::new(),
        }),
    }
}

fn parse_list<T>(root: &serde_json::Value, key: &str, parse: impl Fn(&serde_json::Value) -> T) -> Vec<T> {
    root.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse).collect())
        .unwrap_or_default()
}
