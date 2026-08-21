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
use crate::embedding_runtime::EmbeddingRuntime;
use crate::models::{MemoryKind, SessionStateRecord, WorkingMemoryRecord};
use crate::storage::TursoStorage;

/// 生成召回注入文本（对应 buildDefaultRecallInjection）。
/// embedding 提供语义精排（allowlist 限定在 FTS 候选内）。
pub fn build_recall_injection(
    storage: &TursoStorage,
    prompt: &str,
    project_scope: &str,
    session_id: &str,
    embedding: &mut EmbeddingRuntime,
) -> anyhow::Result<String> {
    let _working = storage.list_working_by_project(project_scope)?;
    let _episodic = storage.list_episodic_by_project(project_scope)?;

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

    // embedding 精排：allowlist = FTS 候选（SIMD 内核内过滤，取代 TS 版全扫描）
    let embedding_candidates = embedding
        .build_embedding_candidates(prompt, &fts_memories)?
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

    // 保持 TS 版顺序：fts → embedding → structured
    let results = recall_memories(RecallMemoriesOptions {
        project_scope,
        limit: 6,
        fts_candidates: &fts_candidates,
        embedding_candidates: &embedding_candidates,
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

/// 节奏 retain（对应 retainDefaultTurnMemories）。embedding 可用时写入向量。
/// `llm_result`：TS 侧调 LLM 生成的结构化 JSON（对应 extractStructuredMemories 的 LLM 路径）；
/// 为 None 时走规则启发式。
pub fn retain_turn_memories(
    storage: &TursoStorage,
    turn_index: i64,
    text: &str,
    session_id: &str,
    project_scope: &str,
    embedding: &mut EmbeddingRuntime,
    llm_result: Option<&str>,
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

    // 结构化提取：LLM 结果优先（解析失败降级规则路径）
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
            // 补全 LLM 结果缺失的 id/session/source 字段
            let mut parsed = parsed;
            for fact in &mut parsed.facts {
                if fact.fact_id.is_empty() {
                    fact.fact_id = format!("{memory_id}:fact:llm");
                }
                fact.source_memory_id = Some(memory_id.clone());
                fact.session_id = session_id.to_string();
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

    // embedding 入库（失败不阻塞 retain 主流程）
    let _ = embedding.save_embedding(storage, MemoryKind::Working, &memory_id, text);
    Ok(())
}

/// 会话整合（对应 consolidateDefaultMemories）。
/// `llm_result`：LLM 生成的摘要文本（Some 时作为 episodic content，degraded=false）；
/// `llm_failed`：LLM 调用失败（degraded=true，内容用规则拼接）。
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

    // LLM 摘要覆盖 content；degraded 标志与 TS 版语义对齐（LLM 失败才 true）
    if let Some(llm_text) = llm_result {
        if !llm_text.trim().is_empty() {
            if let Some(episodic) = consolidated.episodic_memories.first_mut() {
                episodic.content = llm_text.to_string();
            }
        }
    }
    consolidated.degraded = llm_failed;

    // 落库 episodic（对应 runConsolidationOrchestrator）
    // 同 session 多次 consolidate 时递增序号，避免固定 id 互相覆盖
    let existing = storage.count_episodic_by_session(session_id)?;
    for (offset, episodic) in consolidated.episodic_memories.iter_mut().enumerate() {
        episodic.id = format!("{session_id}:episodic:{}", existing + offset as i64);
        storage.insert_episodic(episodic)?;
    }
    // 标记 consolidated_at（对应 TS 版 UPDATE 循环）
    let consolidated_at = now_iso();
    storage.mark_working_consolidated(&consolidated.working_memory_ids, &consolidated_at)?;

    Ok(ConsolidateResult {
        degraded: consolidated.degraded,
        working_memory_ids: consolidated.working_memory_ids.clone(),
        episodic_count: consolidated.episodic_memories.len(),
    })
}

/// 整合源文本：当前 session 未 consolidated 的 working 内容（LLM 输入，对应 TS 版 join("；")）。
pub fn consolidate_source_text(storage: &TursoStorage, session_id: &str, project_scope: &str) -> String {
    storage
        .list_working_by_project(project_scope)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.session_id == session_id && m.consolidated_at.is_none())
        .map(|m| m.content)
        .collect::<Vec<_>>()
        .join("；")
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
