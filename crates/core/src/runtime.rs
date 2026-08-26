//! Runtime orchestration layer (the rule path of the TS `app/runtime.ts`).
//!
//! Exposes four business entry points for JSON-RPC dispatch: build_recall_injection /
//! retain_turn_memories / consolidate_session_memories / memory_command.

use crate::core::consolidation::{
    consolidate_session_memories as consolidate_core, ConsolidateSessionMemoriesOptions,
    WorkingMemoryForConsolidation,
};
use crate::core::extraction::{
    extract_with_heuristics, ExtractStructuredMemoriesOptions, EPOCH_ISO,
};
use crate::core::recall::{recall_memories, RecallCandidateInput, RecallMemoriesOptions};
use crate::embedding_runtime::{ProjectVectors, SharedEmbedder};
use crate::models::{MemoryKind, SessionStateRecord, WorkingMemoryRecord};
use crate::storage::TursoStorage;

/// Build the recall injection text (matches buildDefaultRecallInjection).
/// Embedding provides semantic re-ranking (allowlist limited to FTS candidates).
pub fn build_recall_injection(
    storage: &TursoStorage,
    prompt: &str,
    project_scope: &str,
    session_id: &str,
    shared: &SharedEmbedder,
    vectors: &mut ProjectVectors,
) -> anyhow::Result<String> {
    let structured = query_structured_candidates(storage, prompt, project_scope)?;

    let safe_fts_query = to_safe_fts_query(prompt);
    let mut fts_candidates = Vec::new();
    let mut fts_memories: Vec<(MemoryKind, String)> = Vec::new();
    let working_hits = storage.search_working_fts(&safe_fts_query, project_scope)?;
    for (memory, _score) in &working_hits {
        fts_memories.push((MemoryKind::Working, memory.id.clone()));
    }
    fts_candidates.extend(rank_fts_candidates(&working_hits, MemoryKind::Working));
    let episodic_hits = storage.search_episodic_fts(&safe_fts_query, project_scope)?;
    for (memory, _score) in &episodic_hits {
        fts_memories.push((MemoryKind::Episodic, memory.id.clone()));
    }
    fts_candidates.extend(rank_episodic_candidates(&episodic_hits, MemoryKind::Episodic));

    // embedding re-rank: allowlist = FTS candidates (filtered inside the SIMD kernel, replacing the TS full scan)
    let embedding_candidates = vectors
        .build_embedding_candidates(shared, prompt, &fts_memories)?
        .into_iter()
        .map(|c| RecallCandidateInput {
            memory_id: c.memory_id,
            memory_kind: c.memory_kind,
            project_scope: project_scope.to_string(),
            content: c.content,
            fts_score: None,
            embedding_score: Some(c.score),
            structured_score: None,
        })
        .collect::<Vec<_>>();

    // Keep the TS ordering: fts → embedding → structured
    let results = recall_memories(RecallMemoriesOptions {
        project_scope,
        limit: 6,
        fts_candidates: &fts_candidates,
        embedding_candidates: &embedding_candidates,
        structured_candidates: &structured,
    });

    // Update has_recalled_for_first_turn (matching the TS version)
    let state = storage.get_session_state(session_id)?;
    storage.save_session_state(&SessionStateRecord {
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        last_retained_user_turn: state.as_ref().map(|s| s.last_retained_user_turn).unwrap_or(0),
        has_recalled_for_first_turn: true,
    })?;

    // Render: results.map(r => `- [${kind}] ${content}`).join("\n")
    // working/episodic 存的是完整对话原文，注入前截断避免撑爆上下文；structured 内容短且语义完整，不截断
    let rendered = results
        .iter()
        .map(|candidate| {
            let content = truncate_recall_content(&candidate.content, candidate.memory_kind);
            format!("- [{}] {}", candidate.memory_kind.as_str(), content)
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(rendered)
}

/// Rhythm retain (matches retainDefaultTurnMemories). Writes vectors when embedding is available.
/// `llm_result`: structured JSON produced by the TS-side LLM (the LLM path of extractStructuredMemories);
/// falls back to rule heuristics when None.
#[allow(clippy::too_many_arguments)]
pub fn retain_turn_memories(
    storage: &TursoStorage,
    turn_index: i64,
    text: &str,
    session_id: &str,
    project_scope: &str,
    shared: &SharedEmbedder,
    vectors: &mut ProjectVectors,
    llm_result: Option<&str>,
) -> anyhow::Result<()> {
    let current = storage.get_session_state(session_id)?;
    let memory_id = format!("{session_id}:turn:{turn_index}");
    // Idempotent: keyed on data existence (turnIndex may start at 0, so a default state value of 0 would wrongly skip the first turn)
    if storage.get_working(&memory_id)?.is_some() {
        return Ok(());
    }

    let timestamp = now_iso();
    storage.insert_working(&WorkingMemoryRecord {
        id: memory_id.clone(),
        content: text.to_string(),
        embed_text: Some(text.to_string()),
        source: Some("turn_end".into()),
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        importance: 0.5,
        memory_type: "working".into(),
        metadata: Some(serde_json::json!({ "turnIndex": turn_index })),
        created_at: timestamp.clone(),
        updated_at: timestamp,
        consolidated_at: None,
        valid_until: None,
        superseded_by: None,
    })?;

    // Structured extraction: LLM result first (falls back to rule path on parse failure)
    let mut structured = None;
    if let Some(payload) = llm_result {
        let parsed = crate::llm::parse_llm_extraction(payload);
        if !parsed.facts.is_empty() || !parsed.preferences.is_empty()
            || !parsed.instructions.is_empty() || !parsed.timeline.is_empty()
        {
            structured = Some(parsed);
        }
    }
    let mut extracted = match structured {
        Some(parsed) => {
            // Fill in id/session/source fields missing from the LLM result
            let mut parsed = parsed;
            for fact in &mut parsed.facts {
                if fact.fact_id.is_empty() {
                    fact.fact_id = format!("{memory_id}:fact:llm");
                }
                fact.source_memory_id = Some(memory_id.clone());
                fact.session_id = session_id.to_string();
                fact.project_scope = project_scope.to_string();
                if fact.timestamp.is_empty() {
                    fact.timestamp = crate::core::extraction::EPOCH_ISO.to_string();
                }
            }
            for preference in &mut parsed.preferences {
                if preference.id.is_empty() {
                    preference.id = format!("{memory_id}:preference:llm");
                }
                preference.source_memory_id = Some(memory_id.clone());
                preference.session_id = session_id.to_string();
                preference.project_scope = project_scope.to_string();
                if preference.created_at.is_empty() {
                    preference.created_at = crate::core::extraction::EPOCH_ISO.to_string();
                }
            }
            for instruction in &mut parsed.instructions {
                if instruction.id.is_empty() {
                    instruction.id = format!("{memory_id}:instruction:llm");
                }
                instruction.source_memory_id = Some(memory_id.clone());
                instruction.session_id = session_id.to_string();
                instruction.project_scope = project_scope.to_string();
                if instruction.created_at.is_empty() {
                    instruction.created_at = crate::core::extraction::EPOCH_ISO.to_string();
                }
            }
            for event in &mut parsed.timeline {
                if event.event_id.is_empty() {
                    event.event_id = format!("{memory_id}:timeline:llm");
                }
                event.source = Some("llm".into());
                event.source_memory_id = Some(memory_id.clone());
                event.session_id = session_id.to_string();
                event.project_scope = project_scope.to_string();
                if event.created_at.is_empty() {
                    event.created_at = crate::core::extraction::EPOCH_ISO.to_string();
                }
            }
            parsed
        }
        None => {
            let rule = extract_with_heuristics(&ExtractStructuredMemoriesOptions {
                retained_turn_id: memory_id.clone(),
                session_id: session_id.to_string(),
                project_scope: project_scope.to_string(),
                text: text.to_string(),
            });
            crate::llm::LlmExtractionResult {
                facts: rule.facts,
                preferences: rule.preferences,
                instructions: rule.instructions,
                timeline: rule.timeline,
            }
        }
    };
    storage.insert_facts(&extracted.facts)?;
    storage.insert_preferences(&extracted.preferences)?;
    storage.insert_instructions(&extracted.instructions)?;
    storage.insert_timeline_events(&extracted.timeline)?;
    let _ = &mut extracted;

    storage.save_session_state(&SessionStateRecord {
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        last_retained_user_turn: turn_index,
        has_recalled_for_first_turn: current.as_ref().map(|s| s.has_recalled_for_first_turn).unwrap_or(false),
    })?;

    // Persist the embedding (failure does not block the retain main flow)
    let _ = vectors.save_embedding(storage, shared, MemoryKind::Working, &memory_id, text);
    Ok(())
}

/// Session consolidation (matches consolidateDefaultMemories).
/// `llm_result`: summary text generated by the LLM (used as episodic content when Some, degraded=false);
/// `llm_failed`: LLM call failed (degraded=true, content assembled by rules).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsolidateResult {
    pub degraded: bool,
    pub working_memory_ids: Vec<String>,
    pub episodic_count: usize,
}

pub fn consolidate_session_memories(
    storage: &TursoStorage,
    session_id: &str,
    project_scope: &str,
    llm_result: Option<&str>,
    llm_failed: bool,
) -> anyhow::Result<ConsolidateResult> {
    let working = storage
        .list_working_by_project(project_scope)?
        .into_iter()
        .filter(|m| m.session_id == session_id && m.consolidated_at.is_none())
        .map(|m| WorkingMemoryForConsolidation {
            id: m.id.clone(),
            content: m.content.clone(),
            created_at: m.created_at.clone(),
            updated_at: m.updated_at.clone(),
            importance: m.importance,
        })
        .collect::<Vec<_>>();

    if working.is_empty() {
        return Ok(ConsolidateResult {
            degraded: false,
            working_memory_ids: vec![],
            episodic_count: 0,
        });
    }

    let mut consolidated = consolidate_core(&ConsolidateSessionMemoriesOptions {
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        working_memories: working,
    });

    // LLM summary overrides content; the degraded flag matches TS semantics (true only when LLM fails)
    if let Some(llm_text) = llm_result {
        if !llm_text.trim().is_empty() {
            if let Some(episodic) = consolidated.episodic_memories.first_mut() {
                episodic.content = llm_text.to_string();
            }
        }
    }
    consolidated.degraded = llm_failed;

    // Persist episodic records (matches runConsolidationOrchestrator)
    // Multiple consolidations of one session increment the suffix so fixed ids never overwrite each other
    let existing = storage.count_episodic_by_session(session_id)?;
    for (offset, episodic) in consolidated.episodic_memories.iter_mut().enumerate() {
        episodic.id = format!("{session_id}:episodic:{}", existing + offset as i64);
        storage.insert_episodic(episodic)?;
    }
    // Mark consolidated_at (matches the TS UPDATE loop)
    let consolidated_at = now_iso();
    storage.mark_working_consolidated(&consolidated.working_memory_ids, &consolidated_at)?;

    Ok(ConsolidateResult {
        degraded: consolidated.degraded,
        working_memory_ids: consolidated.working_memory_ids.clone(),
        episodic_count: consolidated.episodic_memories.len(),
    })
}

/// Source text for consolidation: the current session's un-consolidated working content (LLM input, matches TS join(";")).
pub fn consolidate_source_text(storage: &TursoStorage, session_id: &str, project_scope: &str) -> String {
    storage
        .list_working_by_project(project_scope)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.session_id == session_id && m.consolidated_at.is_none())
        .map(|m| m.content)
        .collect::<Vec<_>>()
        .join(";")
}

/// /memory command handling (matches createDefaultMemoryAdmin).
pub fn memory_command(
    storage: &TursoStorage,
    sub: &str,
    target: &str,
    project_scope: &str,
    session_id: &str,
    embedding_status: &str,
) -> anyhow::Result<String> {
    match sub {
        "view" => {
            let working = storage.list_working_by_project(project_scope)?;
            let episodic = storage.list_episodic_by_project(project_scope)?;
            let mut lines = vec![format!("view: project={project_scope}")];
            for memory in working.iter().take(3) {
                lines.push(format!("working:{}:{}", memory.id, memory.content));
            }
            for memory in episodic.iter().take(3) {
                lines.push(format!("episodic:{}:{}", memory.id, memory.content));
            }
            Ok(lines.join("\n"))
        }
        "stats" => {
            let working = storage.count_working_by_project(project_scope)?;
            let episodic = storage.count_episodic_by_project(project_scope)?;
            let facts = storage.count_by_session("facts", session_id)?;
            let preferences = storage.count_by_session("preferences", session_id)?;
            let instructions = storage.count_by_session("instructions", session_id)?;
            let timeline = storage.count_by_session("timeline", session_id)?;
            Ok(format!(
                "stats: working={working} episodic={episodic} facts={facts} preferences={preferences} instructions={instructions} timeline={timeline}"
            ))
        }
        "diagnose" => {
            let db_path = storage.db_path();
            let state = storage.get_session_state(session_id)?;
            let recalled = state.map(|s| s.has_recalled_for_first_turn).unwrap_or(false);
            Ok(format!(
                "diagnose: db={db_path} embedding={embedding_status} llm=off recalled={}",
                if recalled { "yes" } else { "no" }
            ))
        }
        "clear" => {
            let scope = if target.is_empty() { project_scope } else { target };
            storage.clear_project_memories(scope)?;
            Ok(format!("clear: project={scope}"))
        }
        "rebuild" => {
            storage.rebuild_fts()?;
            Ok(format!("rebuild: project={project_scope}"))
        }
        _ => Ok(format!("unknown subcommand: {sub}")),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// 注入渲染时单条候选 content 的最大字符数（按 Unicode 字符计）。
const RECALL_TRUNCATE_CHARS: usize = 300;

/// 截断 recall 注入内容：working/episodic 超长时保留前 300 字符并追加省略号，
/// 其余类型（facts/preferences/instructions/timeline）内容短且语义完整，原样返回。
fn truncate_recall_content(content: &str, kind: MemoryKind) -> String {
    match kind {
        MemoryKind::Working | MemoryKind::Episodic if content.chars().count() > RECALL_TRUNCATE_CHARS => {
            let truncated: String = content.chars().take(RECALL_TRUNCATE_CHARS).collect();
            format!("{truncated}…")
        }
        _ => content.to_string(),
    }
}

fn to_safe_fts_query(prompt: &str) -> String {
    // Matches toSafeFtsQuery: split on whitespace, quote tokens, join with OR
    let tokens: Vec<String> = prompt
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect();
    if tokens.is_empty() {
        format!("\"{}\"", prompt.replace('"', "\"\""))
    } else {
        tokens.join(" OR ")
    }
}

fn rank_fts_candidates(
    hits: &[(WorkingMemoryRecord, f64)],
    kind: MemoryKind,
) -> Vec<RecallCandidateInput> {
    // Matches rankFtsCandidates: ftsScore = total === 0 ? 0 : 1 - index/(total+1)
    let total = hits.len();
    hits.iter()
        .enumerate()
        .map(|(index, (memory, _score))| RecallCandidateInput {
            memory_id: memory.id.clone(),
            memory_kind: kind,
            project_scope: memory.project_scope.clone(),
            content: memory.content.clone(),
            fts_score: Some(if total == 0 { 0.0 } else { 1.0 - index as f64 / (total as f64 + 1.0) }),
            embedding_score: None,
            structured_score: None,
        })
        .collect()
}

fn rank_episodic_candidates(
    hits: &[(crate::models::EpisodicMemoryRecord, f64)],
    kind: MemoryKind,
) -> Vec<RecallCandidateInput> {
    let total = hits.len();
    hits.iter()
        .enumerate()
        .map(|(index, (memory, _score))| RecallCandidateInput {
            memory_id: memory.id.clone(),
            memory_kind: kind,
            project_scope: memory.project_scope.clone(),
            content: memory.content.clone(),
            fts_score: Some(if total == 0 { 0.0 } else { 1.0 - index as f64 / (total as f64 + 1.0) }),
            embedding_score: None,
            structured_score: None,
        })
        .collect()
}

fn query_structured_candidates(
    storage: &TursoStorage,
    query_text: &str,
    project_scope: &str,
) -> anyhow::Result<Vec<RecallCandidateInput>> {
    // Matches queryStructuredCandidates: facts/instructions/preferences/timeline LIKE queries
    let mut candidates = Vec::new();
    for fact in storage.search_facts_like(query_text, project_scope)? {
        candidates.push(RecallCandidateInput {
            memory_id: fact.fact_id,
            memory_kind: MemoryKind::Fact,
            project_scope: project_scope.to_string(),
            content: format!("{}{}{}", fact.subject, fact.predicate, fact.object),
            fts_score: None,
            embedding_score: None,
            structured_score: Some(0.6),
        });
    }
    for instruction in storage.search_instructions_like(query_text, project_scope)? {
        candidates.push(RecallCandidateInput {
            memory_id: instruction.id,
            memory_kind: MemoryKind::Instruction,
            project_scope: project_scope.to_string(),
            content: instruction.instruction,
            fts_score: None,
            embedding_score: None,
            structured_score: Some(0.5),
        });
    }
    for preference in storage.search_preferences_like(query_text, project_scope)? {
        candidates.push(RecallCandidateInput {
            memory_id: preference.id,
            memory_kind: MemoryKind::Preference,
            project_scope: project_scope.to_string(),
            content: preference.preference,
            fts_score: None,
            embedding_score: None,
            structured_score: Some(0.5),
        });
    }
    for event in storage.search_timeline_like(query_text, project_scope)? {
        candidates.push(RecallCandidateInput {
            memory_id: event.event_id,
            memory_kind: MemoryKind::Timeline,
            project_scope: project_scope.to_string(),
            content: format!("{} {}", event.date, event.description),
            fts_score: None,
            embedding_score: None,
            structured_score: Some(0.5),
        });
    }
    Ok(candidates)
}

pub fn now_iso() -> String {
    // Matches new Date().toISOString()
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[allow(dead_code)]
pub(crate) const _EPOCH: &str = EPOCH_ISO;

#[cfg(test)]
mod tests {
    use super::truncate_recall_content;
    use crate::models::MemoryKind;

    fn long_content(units: usize) -> String {
        "这是一段模拟完整对话原文的长内容，包含代码与 markdown。".repeat(units)
    }

    #[test]
    fn truncates_long_working_content_to_300_chars_with_ellipsis() {
        let content = long_content(30); // ~390 字符 > 300
        let truncated = truncate_recall_content(&content, MemoryKind::Working);
        assert_eq!(truncated.chars().count(), 300 + 1, "300 字符 + 省略号");
        assert!(truncated.ends_with('…'));
        let prefix: String = content.chars().take(10).collect();
        assert!(truncated.starts_with(&prefix));
    }

    #[test]
    fn truncates_long_episodic_content() {
        let content = long_content(30);
        let truncated = truncate_recall_content(&content, MemoryKind::Episodic);
        assert_eq!(truncated.chars().count(), 301);
        assert!(truncated.ends_with('…'));
    }

    #[test]
    fn keeps_short_content_unchanged() {
        let content = "短内容".to_string();
        assert_eq!(truncate_recall_content(&content, MemoryKind::Working), content);
        assert_eq!(truncate_recall_content(&content, MemoryKind::Episodic), content);
    }

    #[test]
    fn structured_kinds_are_never_truncated() {
        let content = long_content(50); // 远超过 300 字符
        for kind in [
            MemoryKind::Fact,
            MemoryKind::Preference,
            MemoryKind::Instruction,
            MemoryKind::Timeline,
        ] {
            assert_eq!(truncate_recall_content(&content, kind), content);
        }
    }

    #[test]
    fn truncation_respects_unicode_char_boundaries() {
        // 中文字符是多字节 UTF-8，截断不得产生半个字符
        let content = long_content(30);
        let truncated = truncate_recall_content(&content, MemoryKind::Working);
        assert!(truncated.is_char_boundary(truncated.len()));
        assert!(String::from_utf8(truncated.into_bytes()).is_ok());
    }
}

