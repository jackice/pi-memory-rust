//! 运行时编排层（对应 TS 版 `app/runtime.ts` 的规则路径）。
//!
//! 提供 build_recall_injection / retain_turn_memories / consolidate_session_memories /
//! memory_command 四个业务入口，供 JSON-RPC 分发。

use crate::core::consolidation::{
    consolidate_session_memories as consolidate_core, ConsolidateSessionMemoriesOptions,
    WorkingMemoryForConsolidation,
};
use crate::core::extraction::{
    extract_with_heuristics, ExtractStructuredMemoriesOptions, EPOCH_ISO,
};
use crate::core::recall::{recall_memories, RecallCandidateInput, RecallMemoriesOptions};
use crate::models::{MemoryKind, SessionStateRecord, WorkingMemoryRecord};
use crate::storage::TursoStorage;

/// 生成召回注入文本（对应 buildDefaultRecallInjection，embedding 路径 M2 接入）。
pub fn build_recall_injection(
    storage: &TursoStorage,
    prompt: &str,
    project_scope: &str,
    session_id: &str,
) -> anyhow::Result<String> {
    // M2 embedding 路径会用到这两个列表（buildEmbeddingCandidates 输入），先保留
    let _working = storage.list_working_by_project(project_scope)?;
    let _episodic = storage.list_episodic_by_project(project_scope)?;

    let structured = query_structured_candidates(storage, prompt, project_scope)?;

    let safe_fts_query = to_safe_fts_query(prompt);
    let mut fts_candidates = Vec::new();
    let working_hits = storage.search_working_fts(&safe_fts_query, project_scope)?;
    fts_candidates.extend(rank_fts_candidates(&working_hits, MemoryKind::Working));
    let episodic_hits = storage.search_episodic_fts(&safe_fts_query, project_scope)?;
    fts_candidates.extend(rank_episodic_candidates(&episodic_hits, MemoryKind::Episodic));

    // 保持 TS 版顺序：fts → embedding（M2）→ structured
    let results = recall_memories(RecallMemoriesOptions {
        project_scope,
        limit: 6,
        fts_candidates: &fts_candidates,
        embedding_candidates: &[],
        structured_candidates: &structured,
    });

    // 更新 has_recalled_for_first_turn（与 TS 版一致）
    let state = storage.get_session_state(session_id)?;
    storage.save_session_state(&SessionStateRecord {
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        last_retained_user_turn: state.as_ref().map(|s| s.last_retained_user_turn).unwrap_or(0),
        has_recalled_for_first_turn: true,
    })?;

    // 渲染：results.map(r => `- [${kind}] ${content}`).join("\n")
    let rendered = results
        .iter()
        .map(|candidate| format!("- [{}] {}", candidate.memory_kind.as_str(), candidate.content))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(rendered)
}

/// 节奏 retain（对应 retainDefaultTurnMemories 的规则路径）。
pub fn retain_turn_memories(
    storage: &TursoStorage,
    turn_index: i64,
    text: &str,
    session_id: &str,
    project_scope: &str,
) -> anyhow::Result<()> {
    let current = storage.get_session_state(session_id)?;
    if current.as_ref().map(|s| s.last_retained_user_turn).unwrap_or(0) >= turn_index {
        return Ok(());
    }

    let timestamp = now_iso();
    let memory_id = format!("{session_id}:turn:{turn_index}");
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

    // 结构化提取（规则路径）并落库
    let structured = extract_with_heuristics(&ExtractStructuredMemoriesOptions {
        retained_turn_id: memory_id,
        session_id: session_id.to_string(),
        text: text.to_string(),
    });
    storage.insert_facts(&structured.facts)?;
    storage.insert_preferences(&structured.preferences)?;
    storage.insert_instructions(&structured.instructions)?;
    storage.insert_timeline_events(&structured.timeline)?;

    storage.save_session_state(&SessionStateRecord {
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        last_retained_user_turn: turn_index,
        has_recalled_for_first_turn: current.as_ref().map(|s| s.has_recalled_for_first_turn).unwrap_or(false),
    })?;
    Ok(())
}

/// 会话整合（对应 consolidateDefaultMemories 的规则路径）。
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

    let result = consolidate_core(&ConsolidateSessionMemoriesOptions {
        session_id: session_id.to_string(),
        project_scope: project_scope.to_string(),
        working_memories: working,
    });

    // 落库 episodic（对应 runConsolidationOrchestrator）
    for episodic in &result.episodic_memories {
        storage.insert_episodic(episodic)?;
    }
    // 标记 consolidated_at（对应 TS 版 UPDATE 循环）
    let consolidated_at = now_iso();
    storage.mark_working_consolidated(&result.working_memory_ids, &consolidated_at)?;

    Ok(ConsolidateResult {
        degraded: result.degraded,
        working_memory_ids: result.working_memory_ids.clone(),
        episodic_count: result.episodic_memories.len(),
    })
}

/// /memory 命令处理（对应 createDefaultMemoryAdmin）。
pub fn memory_command(
    storage: &TursoStorage,
    sub: &str,
    target: &str,
    project_scope: &str,
    session_id: &str,
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
                "diagnose: db={db_path} embedding=off llm=off recalled={}",
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
        _ => Ok(format!("未知子命令: {sub}")),
    }
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

fn to_safe_fts_query(prompt: &str) -> String {
    // 对应 toSafeFtsQuery：空白切 token，引号包裹，OR 连接
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
    // 对应 rankFtsCandidates：ftsScore = total === 0 ? 0 : 1 - index/(total+1)
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
    // 对应 queryStructuredCandidates：facts/instructions/preferences/timeline LIKE 查询
    let mut candidates = Vec::new();
    for fact in storage.search_facts_like(query_text)? {
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
    for instruction in storage.search_instructions_like(query_text)? {
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
    for preference in storage.search_preferences_like(query_text)? {
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
    for event in storage.search_timeline_like(query_text)? {
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
    // 对应 new Date().toISOString()
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[allow(dead_code)]
pub(crate) const _EPOCH: &str = EPOCH_ISO;
