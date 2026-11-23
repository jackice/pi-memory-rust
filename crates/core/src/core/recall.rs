//! Recall multi-source candidate merge/ranking using reciprocal rank fusion.

use crate::models::MemoryKind;

const DEFAULT_RECALL_RELATIVE_CUTOFF: f64 = 0.2;

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
    pub fts_rank: Option<usize>,
    pub embedding_rank: Option<usize>,
    pub structured_rank: Option<usize>,
}

fn positive_env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
        .clamp(0.01, 100.0)
}

struct RrfConfig {
    k: f64,
    fts_weight: f64,
    vector_weight: f64,
    structured_weight: f64,
}

impl RrfConfig {
    fn from_env() -> Self {
        Self {
            k: positive_env_f64("PI_MEMORY_RRF_K", 60.0).clamp(1.0, 1_000.0),
            fts_weight: positive_env_f64("PI_MEMORY_FTS_WEIGHT", 1.0),
            vector_weight: positive_env_f64("PI_MEMORY_VECTOR_WEIGHT", 1.0),
            structured_weight: positive_env_f64("PI_MEMORY_STRUCTURED_WEIGHT", 0.8),
        }
    }

    fn weight(&self, field: ScoreField) -> f64 {
        match field {
            ScoreField::Fts => self.fts_weight,
            ScoreField::Embedding => self.vector_weight,
            ScoreField::Structured => self.structured_weight,
        }
    }
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

#[derive(Debug, Clone)]
pub struct RecallDrop {
    pub candidate: RecallCandidate,
    pub reason: &'static str,
}

pub struct RecallMemoriesOptions<'a> {
    pub project_scope: &'a str,
    pub limit: usize,
    pub fts_candidates: &'a [RecallCandidateInput],
    pub embedding_candidates: &'a [RecallCandidateInput],
    pub structured_candidates: &'a [RecallCandidateInput],
}

/// Merge independent FTS, vector, and structured rankings with RRF, then truncate.
pub fn recall_memories(options: RecallMemoriesOptions) -> Vec<RecallCandidate> {
    recall_memories_with_diagnostics(options).0
}

pub fn recall_memories_with_diagnostics(
    options: RecallMemoriesOptions,
) -> (Vec<RecallCandidate>, Vec<RecallDrop>) {
    if std::env::var("PI_MEMORY_RECALL_MODE").ok().as_deref() == Some("legacy") {
        return recall_memories_legacy(options);
    }
    let candidate_count = options.fts_candidates.len()
        + options.embedding_candidates.len()
        + options.structured_candidates.len();
    let mut merged: Vec<RecallCandidate> = Vec::with_capacity(candidate_count);
    let mut positions = std::collections::HashMap::with_capacity(candidate_count);
    let config = RrfConfig::from_env();

    for (rank, candidate) in ordered_source_candidates(
        options.fts_candidates,
        options.project_scope,
        ScoreField::Fts,
    )
    .into_iter()
    .enumerate()
    {
        merge_candidate(
            &mut merged,
            &mut positions,
            candidate,
            ScoreField::Fts,
            rank + 1,
            &config,
        );
    }
    for (rank, candidate) in ordered_source_candidates(
        options.embedding_candidates,
        options.project_scope,
        ScoreField::Embedding,
    )
    .into_iter()
    .enumerate()
    {
        merge_candidate(
            &mut merged,
            &mut positions,
            candidate,
            ScoreField::Embedding,
            rank + 1,
            &config,
        );
    }
    for (rank, candidate) in ordered_source_candidates(
        options.structured_candidates,
        options.project_scope,
        ScoreField::Structured,
    )
    .into_iter()
    .enumerate()
    {
        merge_candidate(
            &mut merged,
            &mut positions,
            candidate,
            ScoreField::Structured,
            rank + 1,
            &config,
        );
    }

    merged.sort_by(|left, right| {
        // Stable tie-break: memory ID ascending.
        right
            .total_score
            .partial_cmp(&left.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.memory_id.cmp(&right.memory_id))
    });
    let max_score = merged
        .first()
        .map(|candidate| candidate.total_score)
        .unwrap_or(0.0);
    let min_score = std::env::var("PI_MEMORY_RECALL_MIN_SCORE")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(0.012);
    let relative_cutoff = std::env::var("PI_MEMORY_RECALL_RELATIVE_CUTOFF")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
        .unwrap_or(DEFAULT_RECALL_RELATIVE_CUTOFF);
    let mut dropped = Vec::new();
    merged.retain(|candidate| {
        let reason =
            recall_drop_reason(candidate.total_score, max_score, min_score, relative_cutoff);
        if let Some(reason) = reason {
            dropped.push(RecallDrop {
                candidate: candidate.clone(),
                reason,
            });
            false
        } else {
            true
        }
    });
    if merged.len() > options.limit {
        dropped.extend(merged.drain(options.limit..).map(|candidate| RecallDrop {
            candidate,
            reason: "result_limit",
        }));
    }
    (merged, dropped)
}

fn recall_drop_reason(
    score: f64,
    max_score: f64,
    min_score: f64,
    relative_cutoff: f64,
) -> Option<&'static str> {
    if score < min_score {
        Some("min_score")
    } else if relative_cutoff > 0.0 && score < max_score * relative_cutoff {
        Some("relative_cutoff")
    } else {
        None
    }
}

/// Compatibility ranking for the one-release rollback switch. Scope is still filtered before
/// merging so enabling legacy scores cannot reintroduce cross-project memory leakage.
fn recall_memories_legacy(
    options: RecallMemoriesOptions,
) -> (Vec<RecallCandidate>, Vec<RecallDrop>) {
    let capacity = options.fts_candidates.len()
        + options.embedding_candidates.len()
        + options.structured_candidates.len();
    let mut merged = Vec::<RecallCandidate>::with_capacity(capacity);
    let mut positions = std::collections::HashMap::with_capacity(capacity);
    for (field, candidates) in [
        (ScoreField::Fts, options.fts_candidates),
        (ScoreField::Embedding, options.embedding_candidates),
        (ScoreField::Structured, options.structured_candidates),
    ] {
        for candidate in candidates {
            if candidate.project_scope != options.project_scope {
                continue;
            }
            let key = (candidate.memory_kind, candidate.memory_id.clone());
            let index = *positions.entry(key).or_insert_with(|| {
                let index = merged.len();
                merged.push(create_candidate(candidate));
                index
            });
            let current = &mut merged[index];
            current.content = candidate.content.clone();
            match field {
                ScoreField::Fts => {
                    current.score_breakdown.fts = current
                        .score_breakdown
                        .fts
                        .max(candidate.fts_score.unwrap_or(0.0));
                }
                ScoreField::Embedding => {
                    current.score_breakdown.embedding = current
                        .score_breakdown
                        .embedding
                        .max(candidate.embedding_score.unwrap_or(0.0));
                }
                ScoreField::Structured => {
                    current.score_breakdown.structured = current
                        .score_breakdown
                        .structured
                        .max(candidate.structured_score.unwrap_or(0.0));
                }
            }
            current.total_score = 1.0
                + current.score_breakdown.fts * 1.5
                + current.score_breakdown.embedding
                + current.score_breakdown.structured;
        }
    }
    merged.sort_by(|left, right| {
        right
            .total_score
            .partial_cmp(&left.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.memory_id.cmp(&right.memory_id))
    });
    let dropped = if merged.len() > options.limit {
        merged
            .drain(options.limit..)
            .map(|candidate| RecallDrop {
                candidate,
                reason: "result_limit",
            })
            .collect()
    } else {
        Vec::new()
    };
    (merged, dropped)
}

#[derive(Clone, Copy)]
enum ScoreField {
    Fts,
    Embedding,
    Structured,
}

/// Preserve caller-provided ranking when the source score is absent. When a source provides
/// scores for every candidate, derive the rank from those scores and use the memory ID to make
/// equal-score source ranks deterministic.
fn ordered_source_candidates<'a>(
    candidates: &'a [RecallCandidateInput],
    project_scope: &str,
    field: ScoreField,
) -> Vec<&'a RecallCandidateInput> {
    let mut ordered = candidates
        .iter()
        .filter(|candidate| candidate.project_scope == project_scope)
        .collect::<Vec<_>>();
    let score = |candidate: &RecallCandidateInput| match field {
        ScoreField::Fts => candidate.fts_score,
        ScoreField::Embedding => candidate.embedding_score,
        ScoreField::Structured => candidate.structured_score,
    };
    if ordered.iter().all(|candidate| score(candidate).is_some()) {
        ordered.sort_by(|left, right| {
            score(right)
                .unwrap_or_default()
                .partial_cmp(&score(left).unwrap_or_default())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.memory_id.cmp(&right.memory_id))
        });
    }
    ordered
}

fn merge_candidate(
    merged: &mut Vec<RecallCandidate>,
    positions: &mut std::collections::HashMap<(MemoryKind, String), usize>,
    candidate: &RecallCandidateInput,
    field: ScoreField,
    rank: usize,
    config: &RrfConfig,
) {
    let key = (candidate.memory_kind, candidate.memory_id.clone());
    let index = if let Some(index) = positions.get(&key) {
        *index
    } else {
        let index = merged.len();
        merged.push(create_candidate(candidate));
        positions.insert(key, index);
        index
    };
    let current = &mut merged[index];

    current.content = candidate.content.clone();
    let contribution = config.weight(field) / (config.k + rank as f64);
    match field {
        ScoreField::Fts => {
            current.score_breakdown.fts = current.score_breakdown.fts.max(contribution);
            current.score_breakdown.fts_rank = Some(rank);
        }
        ScoreField::Embedding => {
            current.score_breakdown.embedding = current.score_breakdown.embedding.max(contribution);
            current.score_breakdown.embedding_rank = Some(rank);
        }
        ScoreField::Structured => {
            current.score_breakdown.structured =
                current.score_breakdown.structured.max(contribution);
            current.score_breakdown.structured_rank = Some(rank);
        }
    }
    current.total_score = score_candidate(&current.score_breakdown);
}

fn create_candidate(candidate: &RecallCandidateInput) -> RecallCandidate {
    let score_breakdown = RecallScoreBreakdown {
        fts: 0.0,
        embedding: 0.0,
        structured: 0.0,
        fts_rank: None,
        embedding_rank: None,
        structured_rank: None,
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
    score.fts + score.embedding + score.structured
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_cutoff_keeps_single_source_hits_below_multi_source_peak() {
        let single_source_rank_three = 1.0 / 63.0;
        let multi_source_rank_one = 1.0 / 61.0 + 1.0 / 61.0 + 0.8 / 61.0;
        assert_eq!(
            recall_drop_reason(
                single_source_rank_three,
                multi_source_rank_one,
                0.012,
                DEFAULT_RECALL_RELATIVE_CUTOFF,
            ),
            None,
            "one strong vector rank must survive consensus filtering"
        );
        assert_eq!(
            recall_drop_reason(single_source_rank_three, multi_source_rank_one, 0.012, 0.5),
            Some("relative_cutoff"),
            "the former 0.5 default removed independent-source hits"
        );
    }

    fn candidate(
        memory_id: &str,
        kind: MemoryKind,
        scope: &str,
        content: &str,
    ) -> RecallCandidateInput {
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
                ..candidate(
                    "wm-1",
                    MemoryKind::Working,
                    "pi-memory",
                    "SQLite-first decision avoids introducing Turso",
                )
            },
            RecallCandidateInput {
                fts_score: Some(0.95),
                ..candidate(
                    "wm-2",
                    MemoryKind::Working,
                    "other-project",
                    "another project also discussing SQLite",
                )
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
        assert_eq!(
            results.len(),
            1,
            "out-of-scope candidates are removed before fusion"
        );
        assert_eq!(
            results[0].score_breakdown,
            RecallScoreBreakdown {
                fts: 1.0 / 61.0,
                embedding: 0.0,
                structured: 0.0,
                fts_rank: Some(1),
                embedding_rank: None,
                structured_rank: None,
            }
        );
    }

    #[test]
    fn merges_fts_embedding_and_structured_candidates() {
        let fts = vec![
            RecallCandidateInput {
                fts_score: Some(0.7),
                ..candidate(
                    "wm-1",
                    MemoryKind::Working,
                    "pi-memory",
                    "SQLite-first decision avoids introducing Turso",
                )
            },
            RecallCandidateInput {
                fts_score: Some(0.95),
                ..candidate(
                    "wm-2",
                    MemoryKind::Working,
                    "other-project",
                    "another project also discussing SQLite",
                )
            },
        ];
        let embedding = vec![RecallCandidateInput {
            embedding_score: Some(0.8),
            ..candidate(
                "ep-1",
                MemoryKind::Episodic,
                "pi-memory",
                "team confirmed the first release targets Pi only",
            )
        }];
        let structured = vec![RecallCandidateInput {
            structured_score: Some(0.6),
            ..candidate(
                "fact-1",
                MemoryKind::Fact,
                "pi-memory",
                "fact: first release uses local SQLite",
            )
        }];
        let results = recall_memories(RecallMemoriesOptions {
            project_scope: "pi-memory",
            limit: 3,
            fts_candidates: &fts,
            embedding_candidates: &embedding,
            structured_candidates: &structured,
        });
        // FTS/vector rank-1 ties; stable memory ID ordering puts ep-1 first.
        let ids: Vec<&str> = results.iter().map(|c| c.memory_id.as_str()).collect();
        assert_eq!(ids, vec!["ep-1", "wm-1", "fact-1"]);
        assert_eq!(results[1].score_breakdown.fts, 1.0 / 61.0);
        assert_eq!(results[1].score_breakdown.fts_rank, Some(1));
    }

    #[test]
    fn tie_breaks_by_memory_id() {
        let fts = vec![
            RecallCandidateInput {
                fts_score: Some(1.0),
                ..candidate("b", MemoryKind::Working, "pi-memory", "b content")
            },
            RecallCandidateInput {
                fts_score: Some(1.0),
                ..candidate("a", MemoryKind::Working, "pi-memory", "a content")
            },
        ];
        let results = recall_memories(RecallMemoriesOptions {
            project_scope: "pi-memory",
            limit: 2,
            fts_candidates: &fts,
            embedding_candidates: &[],
            structured_candidates: &[],
        });
        assert_eq!(
            results[0].memory_id, "a",
            "same score sorts by memoryId ascending"
        );
        assert_eq!(results[1].memory_id, "b");
    }
}
