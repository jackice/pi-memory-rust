//! LLM prompt templates and result parsing (matches the TS prompts/ + fromJsonPayload).
//!
//! The LLM output is generated on the TS side and passed in as `llmResult` (one-way RPC);
//! Rust parses it and degrades to the rule path when parsing fails.

use crate::models::{FactRecord, InstructionRecord, PreferenceRecord, TimelineEventRecord};

/// Structured extraction prompt (matches extract-structured.md; requires strict JSON output).
pub const EXTRACT_PROMPT: &str = r#"Extract structured memories (facts, preferences, instructions, timeline) from the conversation snippet below. Return **only JSON** (no other text, explanation, or markdown):
{
  "facts": [{"subject": "subject", "predicate": "predicate", "object": "object"}],
  "preferences": [{"preference": "preference text", "topic": "topic"}],
  "instructions": [{"instruction": "instruction text", "topic": "topic"}],
  "timeline": [{"date": "YYYY-MM-DD", "description": "event description"}]
}
Rules: return an empty array for categories you cannot reliably extract; do not fabricate; output the JSON only.

Conversation snippet:"#;

/// Session consolidation prompt (matches consolidate-session.md).
pub const CONSOLIDATE_PROMPT: &str = r#"Summarize this session's working memory into 1 reusable episodic memory; if there is not enough information, return a concise summary and do not block the flow."#;

/// Parse the structured JSON returned by the LLM (matches fromJsonPayload).
/// Returns None when parsing fails (the caller degrades to the rule path).
#[derive(Debug, Clone, Default)]
pub struct LlmExtractionResult {
    pub facts: Vec<FactRecord>,
    pub preferences: Vec<PreferenceRecord>,
    pub instructions: Vec<InstructionRecord>,
    pub timeline: Vec<TimelineEventRecord>,
}

/// Parse the structured JSON returned by the LLM; returns a default empty result on parse failure.
/// Note: id/sessionId fields absent from the LLM output are filled in by the caller (the LLM only provides content fields).
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
            project_scope: String::new(),
            timestamp: String::new(),
        }),
        preferences: parse_list(&parsed, "preferences", |v| PreferenceRecord {
            id: String::new(),
            preference: v.get("preference").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            topic: v.get("topic").and_then(|x| x.as_str()).map(String::from),
            context_snippet: None,
            source_memory_id: None,
            session_id: String::new(),
            project_scope: String::new(),
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
            project_scope: String::new(),
            created_at: String::new(),
        }),
        timeline: parse_list(&parsed, "timeline", |v| TimelineEventRecord {
            event_id: String::new(),
            date: v.get("date").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            description: v.get("description").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            source: None,
            source_memory_id: None,
            session_id: String::new(),
            project_scope: String::new(),
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
