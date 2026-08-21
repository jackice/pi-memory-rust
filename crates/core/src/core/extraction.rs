//! 结构化提取：本地规则启发式（移植自 TS 版 `core/extraction.ts`）。
//!
//! LLM 路径（fromJsonPayload）由 M3 桥接层实现，本模块为纯规则路径
//! （extractWithHeuristics），行为逐位一致。

use crate::models::{FactRecord, InstructionRecord, PreferenceRecord, TimelineEventRecord};

/// `new Date(0).toISOString()` 的固定值
pub const EPOCH_ISO: &str = "1970-01-01T00:00:00.000Z";

#[derive(Debug, Clone)]
pub struct ExtractStructuredMemoriesOptions {
    pub retained_turn_id: String,
    pub session_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StructuredExtractionResult {
    pub facts: Vec<FactRecord>,
    pub preferences: Vec<PreferenceRecord>,
    pub instructions: Vec<InstructionRecord>,
    pub timeline: Vec<TimelineEventRecord>,
}

/// 规则启发式提取（对应 extractWithHeuristics）。
pub fn extract_with_heuristics(options: &ExtractStructuredMemoriesOptions) -> StructuredExtractionResult {
    let mut facts = Vec::new();
    let mut preferences = Vec::new();
    let mut instructions = Vec::new();
    let mut timeline = Vec::new();

    let segments: Vec<&str> = options
        .text
        .split(['\n', '。', '！', '？'])
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .collect();

    for (index, line) in segments.iter().enumerate() {
        // 事实: 前缀
        if let Some(statement) = strip_prefix(line, "事实:") {
            facts.push(create_fact(options, index, statement));
            continue;
        }
        // 偏好: 前缀或推断
        let preference = strip_prefix(line, "偏好:").or_else(|| infer_preference(line));
        if let Some(preference) = preference {
            preferences.push(PreferenceRecord {
                id: format!("{}:preference:{}", options.retained_turn_id, index),
                preference: preference.to_string(),
                topic: Some("user-preference".into()),
                context_snippet: Some((*line).to_string()),
                source_memory_id: Some(options.retained_turn_id.clone()),
                session_id: options.session_id.clone(),
                created_at: EPOCH_ISO.into(),
            });
            continue;
        }
        // 指令: 前缀或推断
        let instruction = strip_prefix(line, "指令:").or_else(|| infer_instruction(line));
        if let Some(instruction) = instruction {
            instructions.push(InstructionRecord {
                id: format!("{}:instruction:{}", options.retained_turn_id, index),
                instruction: instruction.to_string(),
                topic: Some("execution-rule".into()),
                active: true,
                context_snippet: Some((*line).to_string()),
                source_memory_id: Some(options.retained_turn_id.clone()),
                session_id: options.session_id.clone(),
                created_at: EPOCH_ISO.into(),
            });
            continue;
        }
        // 时间线: 前缀优先，无前缀也尝试推断
        let timeline_event = strip_prefix(line, "时间线:")
            .and_then(infer_timeline)
            .or_else(|| infer_timeline(line));
        if let Some(event) = timeline_event {
            timeline.push(TimelineEventRecord {
                event_id: format!("{}:timeline:{}", options.retained_turn_id, index),
                date: event.0,
                description: event.1,
                source: Some("retained-turn".into()),
                source_memory_id: Some(options.retained_turn_id.clone()),
                session_id: options.session_id.clone(),
                created_at: EPOCH_ISO.into(),
            });
            continue;
        }
        if looks_like_fact(line) {
            facts.push(create_fact(options, index, line));
        }
    }

    StructuredExtractionResult { facts, preferences, instructions, timeline }
}

fn create_fact(options: &ExtractStructuredMemoriesOptions, index: usize, statement: &str) -> FactRecord {
    let parsed = parse_fact_statement(statement);
    FactRecord {
        fact_id: format!("{}:fact:{}", options.retained_turn_id, index),
        subject: parsed.0,
        predicate: parsed.1,
        object: parsed.2,
        confidence: 0.8,
        source_memory_id: Some(options.retained_turn_id.clone()),
        session_id: options.session_id.clone(),
        timestamp: EPOCH_ISO.into(),
    }
}

fn strip_prefix<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    line.strip_prefix(prefix).map(str::trim)
}

fn infer_preference(line: &str) -> Option<&str> {
    if !line.contains("喜欢") && !line.contains("偏好") {
        return None;
    }
    // 对应 JS `/^我更?/`：剥离「我」或「我更」
    let stripped = line.strip_prefix("我更").or_else(|| line.strip_prefix("我")).unwrap_or(line);
    Some(stripped.trim())
}

fn infer_instruction(line: &str) -> Option<&str> {
    const MARKERS: [&str; 4] = ["不要", "只支持", "必须", "应当"];
    if !MARKERS.iter().any(|marker| line.contains(marker)) {
        return None;
    }
    Some(line.trim())
}

fn infer_timeline(line: &str) -> Option<(String, String)> {
    // 对应 JS `/(\d{4}-\d{2}-\d{2})\s*(.*)/`；调用方已剥离「时间线:」前缀
    let date = line.get(0..10)?;
    if !is_date_like(date) {
        return None;
    }
    let description = line.get(10..).unwrap_or("").trim().to_string();
    Some((date.to_string(), description))
}

fn is_date_like(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}

fn looks_like_fact(statement: &str) -> bool {
    ["正式采用", "采用", "是"].iter().any(|marker| statement.contains(marker))
}

fn parse_fact_statement(statement: &str) -> (String, String, String) {
    for marker in ["正式采用", "采用", "是"] {
        if let Some(index) = statement.find(marker) {
            if index > 0 {
                return (
                    statement[..index].trim().to_string(),
                    marker.to_string(),
                    statement[index + marker.len()..].trim().to_string(),
                );
            }
        }
    }
    // 兜底：subject = object = statement，predicate = "记录为"
    (statement.to_string(), "记录为".to_string(), statement.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(text: &str) -> ExtractStructuredMemoriesOptions {
        ExtractStructuredMemoriesOptions {
            retained_turn_id: "wm-1".into(),
            session_id: "session-1".into(),
            text: text.into(),
        }
    }

    #[test]
    fn extracts_facts_preferences_instructions_and_timeline() {
        let result = extract_with_heuristics(&options(
            "事实: 首版正式采用本地 SQLite\n\
             偏好: 默认自动 recall，不要手动命令驱动\n\
             指令: 不要引入 Turso/libSQL\n\
             时间线: 2026-08-19 完成 Task 2 的 SQLite schema",
        ));

        assert_eq!(result.facts.len(), 1);
        let fact = &result.facts[0];
        assert_eq!(fact.subject, "首版");
        assert_eq!(fact.predicate, "正式采用");
        assert_eq!(fact.object, "本地 SQLite");
        assert_eq!(fact.source_memory_id.as_deref(), Some("wm-1"));
        assert_eq!(fact.confidence, 0.8);
        assert_eq!(fact.timestamp, EPOCH_ISO);

        assert_eq!(result.preferences.len(), 1);
        assert_eq!(result.preferences[0].preference, "默认自动 recall，不要手动命令驱动");
        assert_eq!(result.preferences[0].topic.as_deref(), Some("user-preference"));

        assert_eq!(result.instructions.len(), 1);
        assert_eq!(result.instructions[0].instruction, "不要引入 Turso/libSQL");
        assert!(result.instructions[0].active);

        assert_eq!(result.timeline.len(), 1);
        assert_eq!(result.timeline[0].date, "2026-08-19");
        assert_eq!(result.timeline[0].description, "完成 Task 2 的 SQLite schema");
    }

    #[test]
    fn infers_from_plain_lines() {
        // 无前缀但含标记的行
        let result = extract_with_heuristics(&options("我更喜欢命令行\n不要修改 schema\n2026-08-20 发布了 v0.1.2"));
        assert_eq!(result.preferences.len(), 1);
        assert_eq!(result.preferences[0].preference, "喜欢命令行"); // 「我更」→ 剥离
        assert_eq!(result.instructions.len(), 1);
        assert_eq!(result.timeline.len(), 1);
        assert_eq!(result.timeline[0].date, "2026-08-20");
    }

    #[test]
    fn splits_on_chinese_punctuation() {
        let result = extract_with_heuristics(&options("事实: 第一件事。事实: 第二件事！"));
        assert_eq!(result.facts.len(), 2);
    }
}
