//! Evaluate recall rankings against a JSONL set of relevant memory IDs.
//!
//! Usage: cargo run --release -p pi-memory-core --bin pi-memory-eval -- \
//!   --db /path/to/project.db --scope /project/path [--dataset eval/recall.jsonl]

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};

use anyhow::{bail, Context, Result};
use pi_memory_core::core::recall::{
    recall_memories_with_diagnostics, RecallCandidateInput, RecallMemoriesOptions,
};
use pi_memory_core::embedding_runtime::{EmbeddingProvider, ProjectVectors, SharedEmbedder};
use pi_memory_core::models::MemoryKind;
use pi_memory_core::models::WorkingMemoryRecord;
use pi_memory_core::runtime::{collect_recall_sources, filter_recall_results};
use pi_memory_core::storage::TursoStorage;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EvalCase {
    query: String,
    relevant_ids: Vec<String>,
    language: String,
    #[serde(default)]
    kinds: Vec<String>,
}

fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let db = arg(&args, "--db").context("missing --db")?;
    let scope = arg(&args, "--scope").context("missing --scope")?;
    let dataset = arg(&args, "--dataset").unwrap_or_else(|| "eval/recall.jsonl".into());
    let corpus = arg(&args, "--corpus").unwrap_or_else(|| "eval/corpus.jsonl".into());
    let explain_misses = args.iter().any(|argument| argument == "--explain-misses");
    let cases = read_cases(&dataset)?;
    if cases.is_empty() {
        bail!("evaluation dataset is empty: {dataset}");
    }

    let storage = TursoStorage::open(&db)?;
    let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
    pi_memory_core::model_download::ensure_model_dir(&model_dir)?;
    let shared = SharedEmbedder::new();
    let cold_start = std::time::Instant::now();
    shared.warmup();
    shared.wait_loading(std::time::Duration::from_secs(300));
    let cold_start_ms = cold_start.elapsed().as_secs_f64() * 1_000.0;
    let manifest = pi_memory_core::model_manifest::expected_manifest(&model_dir)?;
    let model_size_bytes = manifest
        .files
        .iter()
        .map(|file| {
            std::fs::metadata(model_dir.join(&file.name))
                .with_context(|| format!("read model file metadata for {}", file.name))
                .map(|metadata| metadata.len())
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum::<u64>();
    let mut vectors = ProjectVectors::new()?;
    if args.iter().any(|argument| argument == "--seed-fixture") {
        if !shared.ensure_loaded() {
            bail!("--seed-fixture requires a ready embedding model");
        }
        seed_fixture(&storage, &shared, &mut vectors, &corpus, &scope)?;
    }
    validate_relevant_ids(&storage, &cases, &scope)?;
    vectors.hydrate(&storage)?;
    eprintln!(
        "model_status={}; vector_count={}",
        shared.status(),
        vectors.index_len()
    );

    let mut totals = [Metrics::default(); 3];
    let mut language_totals = std::collections::BTreeMap::<String, [Metrics; 3]>::new();
    let mut recall_latencies_ms = Vec::with_capacity(cases.len());
    let mut language_latencies_ms = std::collections::BTreeMap::<String, Vec<f64>>::new();
    let mut languages = std::collections::BTreeMap::<String, usize>::new();
    let mut kinds = std::collections::BTreeMap::<String, usize>::new();
    for case in &cases {
        *languages.entry(case.language.clone()).or_default() += 1;
        for kind in &case.kinds {
            *kinds.entry(kind.clone()).or_default() += 1;
        }
        let recall_started = std::time::Instant::now();
        let candidates =
            collect_recall_sources(&storage, &case.query, &scope, &shared, &mut vectors)?;
        let (hybrid_candidates, threshold_drops) =
            recall_memories_with_diagnostics(RecallMemoriesOptions {
                project_scope: &scope,
                limit: if std::env::var("PI_MEMORY_RECALL_MODE").ok().as_deref() == Some("legacy") {
                    6
                } else {
                    50
                },
                fts_candidates: &candidates.fts,
                embedding_candidates: &candidates.vector,
                structured_candidates: &candidates.structured,
            });
        let (hybrid, eligibility_drops) = filter_recall_results(&storage, hybrid_candidates)?;
        let fts_ids = candidate_ids(&candidates.fts);
        let vector_ids = candidate_ids(&candidates.vector);
        let hybrid_ids = hybrid
            .iter()
            .map(|candidate| key(candidate.memory_kind, &candidate.memory_id))
            .collect::<Vec<_>>();
        if explain_misses {
            for relevant_id in &case.relevant_ids {
                let hybrid_rank = rank_of(&hybrid_ids, relevant_id);
                if hybrid_rank.is_some_and(|rank| rank <= 5) {
                    continue;
                }
                let threshold_reason = threshold_drops
                    .iter()
                    .find(|drop| {
                        key(drop.candidate.memory_kind, &drop.candidate.memory_id) == *relevant_id
                    })
                    .map(|drop| drop.reason);
                let eligibility_reason = eligibility_drops
                    .iter()
                    .find(|drop| key_from_drop(drop) == *relevant_id)
                    .map(|drop| drop.reason.as_str());
                eprintln!(
                    "[miss] language={} target={} fts_rank={:?} vector_rank={:?} hybrid_rank={:?} threshold_drop={:?} eligibility_drop={:?} query={:?}",
                    case.language,
                    relevant_id,
                    rank_of(&fts_ids, relevant_id),
                    rank_of(&vector_ids, relevant_id),
                    hybrid_rank,
                    threshold_reason,
                    eligibility_reason,
                    case.query,
                );
            }
        }
        let rankings = [fts_ids, vector_ids, hybrid_ids];
        let latency_ms = recall_started.elapsed().as_secs_f64() * 1_000.0;
        recall_latencies_ms.push(latency_ms);
        language_latencies_ms
            .entry(case.language.clone())
            .or_default()
            .push(latency_ms);
        let language_metrics = language_totals
            .entry(case.language.clone())
            .or_insert([Metrics::default(); 3]);
        for (index, ranking) in rankings.iter().enumerate() {
            totals[index].add(ranking, &case.relevant_ids);
            language_metrics[index].add(ranking, &case.relevant_ids);
        }
    }
    let p50_ms = percentile(&mut recall_latencies_ms.clone(), 0.50);
    let p95_ms = percentile(&mut recall_latencies_ms, 0.95);
    println!(
        "runtime: model_id={} model_size_bytes={} cold_start_ms={:.2} recall_latency_p50_ms={:.2} recall_latency_p95_ms={:.2}",
        pi_memory_core::embedding_runtime::active_model_id(),
        model_size_bytes,
        cold_start_ms,
        p50_ms,
        p95_ms,
    );
    eprintln!("dataset_cases_by_language={languages:?}; kind_tags={kinds:?}");
    for (name, metrics) in ["fts-only", "vector-only", "hybrid"]
        .into_iter()
        .zip(totals)
    {
        println!(
            "{name}: cases={} Recall@5={:.4} Recall@10={:.4} MRR={:.4} nDCG@10={:.4}",
            cases.len(),
            metrics.recall5 / cases.len() as f64,
            metrics.recall10 / cases.len() as f64,
            metrics.mrr / cases.len() as f64,
            metrics.ndcg10 / cases.len() as f64,
        );
    }
    for (language, language_metrics) in language_totals {
        let case_count = languages.get(&language).copied().unwrap_or(0);
        for (name, metrics) in ["fts-only", "vector-only", "hybrid"]
            .into_iter()
            .zip(language_metrics)
        {
            println!(
                "language={language} {name}: cases={case_count} Recall@5={:.4} Recall@10={:.4} MRR={:.4} nDCG@10={:.4}",
                metrics.recall5 / case_count as f64,
                metrics.recall10 / case_count as f64,
                metrics.mrr / case_count as f64,
                metrics.ndcg10 / case_count as f64,
            );
        }
        if let Some(latencies) = language_latencies_ms.get_mut(&language) {
            let p50 = percentile(&mut latencies.clone(), 0.50);
            let p95 = percentile(latencies, 0.95);
            println!(
                "language={language} recall_latency_p50_ms={p50:.2} recall_latency_p95_ms={p95:.2}"
            );
        }
    }
    Ok(())
}

fn percentile(values: &mut [f64], quantile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let index = ((values.len() as f64 * quantile).ceil() as usize)
        .saturating_sub(1)
        .min(values.len() - 1);
    values[index]
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureMemory {
    id: String,
    content: String,
}

fn seed_fixture(
    storage: &TursoStorage,
    shared: &SharedEmbedder,
    vectors: &mut ProjectVectors,
    path: &str,
    scope: &str,
) -> Result<()> {
    let memories = BufReader::new(File::open(path).with_context(|| format!("open corpus {path}"))?)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            Ok(line) => Some(
                serde_json::from_str::<FixtureMemory>(&line)
                    .with_context(|| format!("parse corpus line {}", index + 1)),
            ),
            Err(error) => Some(Err(error.into())),
        })
        .collect::<Result<Vec<_>>>()?;
    let texts = memories
        .iter()
        .map(|memory| memory.content.clone())
        .collect::<Vec<_>>();
    let embeddings = shared
        .embed_document(&texts)
        .context("embed fixture corpus")?;
    let now = pi_memory_core::runtime::now_iso();
    storage.begin_tx()?;
    let write_result = (|| -> Result<()> {
        for memory in &memories {
            storage.insert_working(&WorkingMemoryRecord {
                id: memory.id.clone(),
                content: memory.content.clone(),
                embed_text: Some(memory.content.clone()),
                source: Some("eval-fixture".into()),
                session_id: "eval-fixture".into(),
                project_scope: scope.into(),
                importance: 0.8,
                memory_type: "working".into(),
                metadata: None,
                created_at: now.clone(),
                updated_at: now.clone(),
                consolidated_at: None,
                valid_until: None,
                superseded_by: None,
            })?;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = storage.rollback_tx();
        return Err(error);
    }
    storage.commit_tx()?;
    let items = memories
        .iter()
        .map(|memory| {
            (
                MemoryKind::Working,
                memory.id.clone(),
                memory.content.clone(),
            )
        })
        .collect::<Vec<_>>();
    vectors.save_embeddings_batch(storage, &items, &embeddings)?;
    storage.rebuild_fts()?;
    Ok(())
}

fn key(kind: MemoryKind, id: &str) -> String {
    format!("{}:{id}", kind.as_str())
}

fn candidate_ids(candidates: &[RecallCandidateInput]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| key(candidate.memory_kind, &candidate.memory_id))
        .collect()
}

fn rank_of(ranking: &[String], memory_id: &str) -> Option<usize> {
    ranking
        .iter()
        .position(|candidate| candidate == memory_id)
        .map(|rank| rank + 1)
}

fn key_from_drop(drop: &pi_memory_core::runtime::RecallDropDiagnostic) -> String {
    format!("{}:{}", drop.memory_kind, drop.memory_id)
}

#[derive(Clone, Copy, Default)]
struct Metrics {
    recall5: f64,
    recall10: f64,
    mrr: f64,
    ndcg10: f64,
}

impl Metrics {
    fn add(&mut self, ranking: &[String], relevant: &[String]) {
        let relevant = relevant.iter().collect::<HashSet<_>>();
        if relevant.is_empty() {
            return;
        }
        let hits5 = ranking
            .iter()
            .take(5)
            .filter(|id| relevant.contains(id))
            .count();
        let hits10 = ranking
            .iter()
            .take(10)
            .filter(|id| relevant.contains(id))
            .count();
        self.recall5 += hits5 as f64 / relevant.len() as f64;
        self.recall10 += hits10 as f64 / relevant.len() as f64;
        if let Some(rank) = ranking.iter().position(|id| relevant.contains(id)) {
            self.mrr += 1.0 / (rank + 1) as f64;
        }
        let dcg = ranking
            .iter()
            .take(10)
            .enumerate()
            .filter(|(_, id)| relevant.contains(id))
            .map(|(rank, _)| 1.0 / ((rank + 2) as f64).log2())
            .sum::<f64>();
        let ideal = (0..relevant.len().min(10))
            .map(|rank| 1.0 / ((rank + 2) as f64).log2())
            .sum::<f64>();
        self.ndcg10 += if ideal == 0.0 { 0.0 } else { dcg / ideal };
    }
}

fn read_cases(path: &str) -> Result<Vec<EvalCase>> {
    let cases = BufReader::new(File::open(path).with_context(|| format!("open dataset {path}"))?)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            Ok(line) => Some(
                serde_json::from_str(&line)
                    .with_context(|| format!("parse dataset line {}", index + 1))
                    .map(|case| (index + 1, case)),
            ),
            Err(error) => Some(Err(error.into())),
        })
        .collect::<Result<Vec<(usize, EvalCase)>>>()?;

    for (line, case) in &cases {
        if case.query.trim().is_empty() {
            bail!("evaluation dataset line {line}: query must not be empty");
        }
        if case.language.trim().is_empty() {
            bail!("evaluation dataset line {line}: language must not be empty");
        }
        if case.relevant_ids.is_empty() {
            bail!("evaluation dataset line {line}: relevantIds must contain at least one ID");
        }

        let mut unique_ids = HashSet::with_capacity(case.relevant_ids.len());
        for id in &case.relevant_ids {
            if id.trim().is_empty() || id.trim() != id {
                bail!("evaluation dataset line {line}: relevantIds contains an empty or padded ID");
            }
            let Some((kind, memory_id)) = id.split_once(':') else {
                bail!("evaluation dataset line {line}: relevant ID {id:?} must use <kind>:<id>");
            };
            if !matches!(
                kind,
                "working" | "episodic" | "fact" | "preference" | "instruction" | "timeline"
            ) || memory_id.trim().is_empty()
            {
                bail!(
                    "evaluation dataset line {line}: relevant ID {id:?} has an unknown kind or empty memory ID"
                );
            }
            if !unique_ids.insert(id) {
                bail!("evaluation dataset line {line}: duplicate relevant ID {id:?}");
            }
        }
    }

    Ok(cases.into_iter().map(|(_, case)| case).collect())
}

fn validate_relevant_ids(storage: &TursoStorage, cases: &[EvalCase], scope: &str) -> Result<()> {
    let mut checked_ids = HashSet::new();
    for case in cases {
        for relevant_id in &case.relevant_ids {
            if !checked_ids.insert(relevant_id) {
                continue;
            }
            let (kind_name, memory_id) = relevant_id
                .split_once(':')
                .context("validated relevant ID lost its kind separator")?;
            let kind = match kind_name {
                "working" => MemoryKind::Working,
                "episodic" => MemoryKind::Episodic,
                "fact" => MemoryKind::Fact,
                "preference" => MemoryKind::Preference,
                "instruction" => MemoryKind::Instruction,
                "timeline" => MemoryKind::Timeline,
                _ => bail!("unsupported relevant ID kind: {kind_name}"),
            };
            if !storage
                .memory_exists_for_project(kind, memory_id, scope)
                .with_context(|| format!("check relevant ID {relevant_id:?}"))?
            {
                bail!("relevant ID {relevant_id:?} does not exist in project scope {scope:?}");
            }
        }
    }
    Ok(())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == name)
        .and_then(|index| args.get(index + 1))
        .cloned()
}
