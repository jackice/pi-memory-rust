//! recall 多路候选合并排序（移植自 TS 版 `core/recall.ts`，bit-exact）。
//!
//! 保持逐位一致的关键点：
//! - 合并顺序：fts → embedding → structured（scopeBoost 每路重新赋值）；
//! - 分数公式：`scopeBoost + fts * 1.5 + embedding + structured`（左结合）；
//! - 排序 tie-break：totalScore 相等时按 memoryId 比较。

use crate::models::MemoryKind;

#[derive(Debug, Clone)]
pub struct RecallCandidateInput {
    pub memory_id: String,
    pub memory_kind: MemoryKind,
    pub project_scope: String,
    pub content: String,
    pub fts_score: Option<f64>,
    pub embedding_score: Option<f64>,
    pub structured_score: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecallScoreBreakdown {
    pub fts: f64,
    pub embedding: f64,
    pub structured: f64,
    pub scope_boost: f64,
}

#[derive(Debug, Clone)]
pub struct RecallCandidate {
    pub memory_id: String,
    pub memory_kind: MemoryKind,
    pub project_scope: String,
    pub content: String,
    pub score_breakdown: RecallScoreBreakdown,
    pub total_score: f64,
}

pub struct RecallMemoriesOptions<'a> {
    pub project_scope: &'a str,
    pub limit: usize,
    pub fts_candidates: &'a [RecallCandidateInput],
    pub embedding_candidates: &'a [RecallCandidateInput],
    pub structured_candidates: &'a [RecallCandidateInput],
}

/// 合并多路候选并排序截断（对应 recallMemories）。
pub fn recall_memories(options: RecallMemoriesOptions) -> Vec<RecallCandidate> {
    let mut merged: Vec<RecallCandidate> = Vec::new();

    for candidate in options.fts_candidates {
        merge_candidate(&mut merged, options.project_scope, candidate, ScoreField::Fts);
    }
    for candidate in options.embedding_candidates {
        merge_candidate(&mut merged, options.project_scope, candidate, ScoreField::Embedding);
    }
    for candidate in options.structured_candidates {
        merge_candidate(&mut merged, options.project_scope, candidate, ScoreField::Structured);
    }

    merged.sort_by(|left, right| {
        // 与 TS 排序一致：totalScore 降序，相等时 memoryId 升序（ASCII 下 localeCompare 同字节序）
        right
            .total_score
            .partial_cmp(&left.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.memory_id.cmp(&right.memory_id))
    });
    merged.truncate(options.limit);
    merged
}

#[derive(Clone, Copy)]
enum ScoreField {
    Fts,
    Embedding,
    Structured,
}

fn merge_candidate(
    merged: &mut Vec<RecallCandidate>,
    project_scope: &str,
    candidate: &RecallCandidateInput,
    field: ScoreField,
) {
    let key = format!("{}:{}", candidate.memory_kind.as_str(), candidate.memory_id);
    let position = merged.iter().position(|c| {
        format!("{}:{}", c.memory_kind.as_str(), c.memory_id) == key
    });

    let current = match position {
        Some(index) => &mut merged[index],
        None => {
            merged.push(create_candidate(project_scope, candidate));
            merged.last_mut().expect("刚 push 必有值")
        }
    };

    current.content = candidate.content.clone();
    match field {
        ScoreField::Fts => {
            current.score_breakdown.fts =
                current.score_breakdown.fts.max(candidate.fts_score.unwrap_or(0.0));
        }
        ScoreField::Embedding => {
            current.score_breakdown.embedding =
                current.score_breakdown.embedding.max(candidate.embedding_score.unwrap_or(0.0));
        }
        ScoreField::Structured => {
            current.score_breakdown.structured =
                current.score_breakdown.structured.max(candidate.structured_score.unwrap_or(0.0));
        }
    }
    current.score_breakdown.scope_boost = (candidate.project_scope == project_scope) as u8 as f64;
    current.total_score = score_candidate(&current.score_breakdown);
}

fn create_candidate(project_scope: &str, candidate: &RecallCandidateInput) -> RecallCandidate {
    let score_breakdown = RecallScoreBreakdown {
        fts: candidate.fts_score.unwrap_or(0.0),
        embedding: candidate.embedding_score.unwrap_or(0.0),
        structured: candidate.structured_score.unwrap_or(0.0),
        scope_boost: (candidate.project_scope == project_scope) as u8 as f64,
    };
    let total_score = score_candidate(&score_breakdown);
    RecallCandidate {
        memory_id: candidate.memory_id.clone(),
        memory_kind: candidate.memory_kind,
        project_scope: candidate.project_scope.clone(),
        content: candidate.content.clone(),
        score_breakdown,
        total_score,
    }
}

fn score_candidate(score: &RecallScoreBreakdown) -> f64 {
    score.scope_boost + score.fts * 1.5 + score.embedding + score.structured
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(memory_id: &str, kind: MemoryKind, scope: &str, content: &str) -> RecallCandidateInput {
        RecallCandidateInput {
            memory_id: memory_id.into(),
            memory_kind: kind,
            project_scope: scope.into(),
            content: content.into(),
            fts_score: None,
            embedding_score: None,
            structured_score: None,
        }
    }

    #[test]
    fn returns_fts_only_results_without_embedding_provider() {
        let fts = vec![
            RecallCandidateInput {
                fts_score: Some(0.7),
                ..candidate("wm-1", MemoryKind::Working, "pi-memory", "SQLite-first 决策避免引入 Turso")
            },
            RecallCandidateInput {
                fts_score: Some(0.95),
                ..candidate("wm-2", MemoryKind::Working, "other-project", "别的项目也在讨论 SQLite")
            },
        ];
        let results = recall_memories(RecallMemoriesOptions {
            project_scope: "pi-memory",
            limit: 2,
            fts_candidates: &fts,
            embedding_candidates: &[],
            structured_candidates: &[],
        });
        assert_eq!(results[0].memory_id, "wm-1");
        assert_eq!(results[1].memory_id, "wm-2");
        // wm-2 的 fts 0.95*1.5 = 1.425 + boost 0；wm-1 0.7*1.5 + 1 = 2.05 → wm-1 在前（scope 优先）
        assert_eq!(results[0].score_breakdown, RecallScoreBreakdown {
            fts: 0.7, embedding: 0.0, structured: 0.0, scope_boost: 1.0,
        });
    }

    #[test]
    fn merges_fts_embedding_and_structured_candidates() {
        let fts = vec![
            RecallCandidateInput {
                fts_score: Some(0.7),
                ..candidate("wm-1", MemoryKind::Working, "pi-memory", "SQLite-first 决策避免引入 Turso")
            },
            RecallCandidateInput {
                fts_score: Some(0.95),
                ..candidate("wm-2", MemoryKind::Working, "other-project", "别的项目也在讨论 SQLite")
            },
        ];
        let embedding = vec![RecallCandidateInput {
            embedding_score: Some(0.8),
            ..candidate("ep-1", MemoryKind::Episodic, "pi-memory", "团队确认首版只支持 Pi")
        }];
        let structured = vec![RecallCandidateInput {
            structured_score: Some(0.6),
            ..candidate("fact-1", MemoryKind::Fact, "pi-memory", "事实：首版使用本地 SQLite")
        }];
        let results = recall_memories(RecallMemoriesOptions {
            project_scope: "pi-memory",
            limit: 3,
            fts_candidates: &fts,
            embedding_candidates: &embedding,
            structured_candidates: &structured,
        });
        // wm-1: 0.7*1.5+1=2.05；ep-1: 0.8+1=1.8；fact-1: 0.6+1=1.6
        let ids: Vec<&str> = results.iter().map(|c| c.memory_id.as_str()).collect();
        assert_eq!(ids, vec!["wm-1", "ep-1", "fact-1"]);
        assert_eq!(results[0].score_breakdown.fts, 0.7);
        assert_eq!(results[0].score_breakdown.scope_boost, 1.0);
    }

    #[test]
    fn tie_breaks_by_memory_id() {
        let fts = vec![
            RecallCandidateInput {
                fts_score: Some(1.0),
                ..candidate("b", MemoryKind::Working, "pi-memory", "b 内容")
            },
            RecallCandidateInput {
                fts_score: Some(1.0),
                ..candidate("a", MemoryKind::Working, "pi-memory", "a 内容")
            },
        ];
        let results = recall_memories(RecallMemoriesOptions {
            project_scope: "pi-memory",
            limit: 2,
            fts_candidates: &fts,
            embedding_candidates: &[],
            structured_candidates: &[],
        });
        assert_eq!(results[0].memory_id, "a", "总分相同按 memoryId 升序");
        assert_eq!(results[1].memory_id, "b");
    }
}
