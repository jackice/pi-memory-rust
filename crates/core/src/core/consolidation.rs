//! 会话整合（移植自 TS 版 `core/consolidation.ts`）。
//!
//! 规则版：无 LLM 时直接拼接 working 记忆内容生成 episodic 记忆。
//! LLM 路径（含超时降级）由 M3 桥接层接入，degraded 标志语义保留。

use crate::core::extraction::EPOCH_ISO;
use crate::models::EpisodicMemoryRecord;

#[derive(Debug, Clone)]
pub struct ConsolidateSessionMemoriesOptions {
    pub session_id: String,
    pub project_scope: String,
    pub working_memories: Vec<WorkingMemoryForConsolidation>,
}

/// 整合所需的 working 记忆字段子集。
#[derive(Debug, Clone)]
pub struct WorkingMemoryForConsolidation {
    pub id: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
    pub importance: f64,
}

#[derive(Debug, Clone)]
pub struct ConsolidationResult {
    pub episodic_memories: Vec<EpisodicMemoryRecord>,
    pub working_memory_ids: Vec<String>,
    pub degraded: bool,
}

/// 规则版整合（对应 consolidateSessionMemories 的降级路径）。
pub fn consolidate_session_memories(
    options: &ConsolidateSessionMemoriesOptions,
) -> ConsolidationResult {
    let working_memory_ids: Vec<String> = options.working_memories.iter().map(|m| m.id.clone()).collect();
    let source_summary = working_memory_ids.join(",");
    // 规则版无 LLM：degraded = false（TS 版仅在 llm 存在且失败时为 true）
    let degraded = false;
    let content = options
        .working_memories
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("；");

    let first = options.working_memories.first();
    let last = options.working_memories.last();
    let importance = options
        .working_memories
        .iter()
        .map(|m| m.importance)
        .fold(0.0_f64, f64::max); // 对应 Math.max(..., 0)：空列表为 0

    let episodic = EpisodicMemoryRecord {
        id: format!("{}:episodic:0", options.session_id),
        content,
        summary_of: source_summary,
        session_id: options.session_id.clone(),
        project_scope: options.project_scope.clone(),
        importance,
        tier: 1,
        memory_type: "episodic".into(),
        metadata: Some(serde_json::json!({ "degraded": degraded })),
        created_at: first.map(|m| m.created_at.clone()).unwrap_or_else(|| EPOCH_ISO.into()),
        updated_at: last.map(|m| m.updated_at.clone()).unwrap_or_else(|| EPOCH_ISO.into()),
        degraded_at: None,
        valid_until: None,
        superseded_by: None,
    };

    ConsolidationResult {
        episodic_memories: vec![episodic],
        working_memory_ids,
        degraded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wm(id: &str, content: &str, importance: f64) -> WorkingMemoryForConsolidation {
        WorkingMemoryForConsolidation {
            id: id.into(),
            content: content.into(),
            created_at: "2026-08-19T03:00:00.000Z".into(),
            updated_at: "2026-08-19T03:00:00.000Z".into(),
            importance,
        }
    }

    #[test]
    fn consolidates_working_memory_into_episodic() {
        let result = consolidate_session_memories(&ConsolidateSessionMemoriesOptions {
            session_id: "session-1".into(),
            project_scope: "pi-memory".into(),
            working_memories: vec![
                wm("wm-1", "实现 SQLite schema 与 session_state", 0.8),
                wm("wm-2", "补齐 SqliteMemoryStore 与 FTS 同步", 0.9),
            ],
        });

        assert!(!result.degraded);
        assert_eq!(result.working_memory_ids, vec!["wm-1", "wm-2"]);
        let episodic = &result.episodic_memories[0];
        assert_eq!(episodic.id, "session-1:episodic:0");
        assert_eq!(episodic.summary_of, "wm-1,wm-2");
        assert_eq!(episodic.memory_type, "episodic");
        assert_eq!(episodic.content, "实现 SQLite schema 与 session_state；补齐 SqliteMemoryStore 与 FTS 同步");
        assert_eq!(episodic.importance, 0.9, "importance 取 max");
        assert_eq!(episodic.metadata, Some(serde_json::json!({ "degraded": false })));
    }

    #[test]
    fn empty_working_memories_uses_epoch_defaults() {
        let result = consolidate_session_memories(&ConsolidateSessionMemoriesOptions {
            session_id: "session-1".into(),
            project_scope: "pi-memory".into(),
            working_memories: vec![],
        });
        assert_eq!(result.episodic_memories[0].importance, 0.0);
        assert_eq!(result.episodic_memories[0].created_at, EPOCH_ISO);
        assert_eq!(result.episodic_memories[0].updated_at, EPOCH_ISO);
        assert_eq!(result.episodic_memories[0].content, "");
    }
}
