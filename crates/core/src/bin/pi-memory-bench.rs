//! Benchmark tool for 10k-scale memories.
//!
//! Usage: `cargo run --release -p pi-memory-core --bin pi-memory-bench [--count 10000] [--queries 100]`
//!
//! What is measured (all local computation, zero LLM API cost):
//! - write: N memories (real retain path: rule extraction + ONNX embedding + turbovec index + Tantivy FTS)
//! - recall: buildRecallInjection latency for M queries (FTS coarse filter + embedding re-rank + multi-source merge)
//!   acceptance: recall injection ≤ 300ms at 10k memories (spec §6).

use std::path::PathBuf;
use std::time::Instant;

use pi_memory_core::core::recall::{recall_memories, RecallCandidateInput, RecallMemoriesOptions};
use pi_memory_core::embedding_runtime::{EmbeddingProvider, ProjectVectors, SharedEmbedder};
use pi_memory_core::models::{
    FactRecord, InstructionRecord, MemoryKind, PreferenceRecord, TimelineEventRecord,
};
use pi_memory_core::paths;
use pi_memory_core::runtime::{build_recall_injection, retain_turn_memories};
use pi_memory_core::storage::TursoStorage;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|argument| argument == "--merge-only") {
        let iterations = arg(&args, "--iterations")
            .unwrap_or(1_000)
            .clamp(1, 100_000);
        benchmark_recall_merge(iterations);
        return;
    }
    let count: usize = arg(&args, "--count").unwrap_or(10_000).max(1);
    let queries: usize = arg(&args, "--queries").unwrap_or(100).max(1);
    let assert_gates = args.iter().any(|argument| argument == "--assert-gates");
    if assert_gates && count < 10_000 {
        eprintln!("[gates] --assert-gates requires at least 10,000 rows per structured table");
        std::process::exit(2);
    }
    let scope = format!("/tmp/pi-memory-bench-{}", std::process::id());

    // Isolated data directory (does not touch real data)
    let data_dir = std::env::temp_dir().join(format!("pi-memory-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let db_path =
        paths::ensure_project_db_dir(&data_dir, &scope).expect("failed to create benchmark db");
    let storage = TursoStorage::open(db_path.to_str().expect("path UTF-8"))
        .expect("failed to open benchmark db");
    let shared = SharedEmbedder::new();

    println!("== pi-memory-rust benchmark ==");
    println!("memories: {count} | queries: {queries}");
    shared.warmup();
    shared.wait_loading(std::time::Duration::from_secs(300));
    let model_ready = shared.ensure_loaded();
    println!("model: {}", shared.status());
    if !model_ready {
        eprintln!(
            "[bench] model not ready (embeddings will degrade; results are FTS-only). Run /memory prepare-model or set PI_MEMORY_MODEL_PATH"
        );
    }

    // Build test texts: templated CJK/English mix so both FTS and embedding have content
    let topics = [
        "SQLite 迁移",
        "Rust 重写",
        "向量检索",
        "全文搜索",
        "会话整合",
        "记忆隔离",
        "embedding 模型",
        "Tantivy 索引",
        "turbovec 量化",
        "JSON-RPC 桥接",
    ];
    let texts: Vec<String> = (0..count)
        .map(|i| {
            format!(
                "事实: 项目{}在{}阶段讨论了{}方案，编号{:05}；偏好: 回复使用中文",
                i % 100,
                topics[i % topics.len()],
                topics[(i * 7) % topics.len()],
                i
            )
        })
        .collect();

    // ---------- write benchmark ----------
    // Bulk fill: batched embed (onnxruntime batches efficiently) + per-row write/index,
    // simulates the system after accumulating 10k memories; the single-turn retain path is measured separately below.
    let mut vectors = ProjectVectors::new().expect("failed to create vector index");
    println!("\n[write] bulk-filling {count} rows (embed batch size 64, tx commit)...");
    let write_start = Instant::now();
    let mut embed_total = std::time::Duration::new(0, 0);
    let batch_size = 64usize;
    storage.begin_tx().expect("failed to begin tx");
    storage
        .drop_fts()
        .expect("failed to drop FTS index (fill phase)");
    let mut all_items: Vec<(pi_memory_core::models::MemoryKind, String, String)> =
        Vec::with_capacity(count);
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(count);
    let mut records: Vec<pi_memory_core::models::WorkingMemoryRecord> = Vec::with_capacity(count);
    for (batch_start, chunk) in texts.chunks(batch_size).enumerate() {
        let t_embed = Instant::now();
        let embeddings = shared.embed_document(chunk).expect("embed failed");
        embed_total += t_embed.elapsed();
        for (offset, (text, embedding)) in chunk.iter().zip(embeddings.iter()).enumerate() {
            let i = batch_start * batch_size + offset;
            let memory_id = format!("bench:{i}");
            all_items.push((
                pi_memory_core::models::MemoryKind::Working,
                memory_id.clone(),
                text.clone(),
            ));
            all_embeddings.push(embedding.clone());
            records.push(pi_memory_core::models::WorkingMemoryRecord {
                id: memory_id,
                content: text.clone(),
                embed_text: Some(text.clone()),
                source: Some("bench".into()),
                session_id: "bench-session".into(),
                project_scope: scope.clone(),
                importance: 0.5,
                memory_type: "working".into(),
                metadata: None,
                created_at: "2026-08-21T00:00:00.000Z".into(),
                updated_at: "2026-08-21T00:00:00.000Z".into(),
                consolidated_at: None,
                valid_until: None,
                superseded_by: None,
            });
        }
        if (batch_start + 1) % 16 == 0 {
            println!(
                "  ... {} rows ({:.1}s)",
                (batch_start + 1) * batch_size,
                write_start.elapsed().as_secs_f64()
            );
        }
    }
    let t_db = Instant::now();
    for record in &records {
        storage.insert_working(record).expect("insert failed");
    }
    let db_elapsed = t_db.elapsed();
    storage
        .commit_tx()
        .expect("failed to commit working memories");
    let t_vec = Instant::now();
    vectors
        .save_embeddings_batch(&storage, &all_items, &all_embeddings)
        .expect("bulk vector persist failed");
    let vec_elapsed = t_vec.elapsed();
    let t_fts = Instant::now();
    storage.rebuild_fts().expect("failed to rebuild FTS index");
    let fts_elapsed = t_fts.elapsed();
    let write_elapsed = write_start.elapsed();
    println!(
        "  [phases] embed {:.1}s | DB write {:.1}s | vector index {:.1}s | FTS rebuild {:.1}s",
        embed_total.as_secs_f64(),
        db_elapsed.as_secs_f64(),
        vec_elapsed.as_secs_f64(),
        fts_elapsed.as_secs_f64()
    );
    println!(
        "[write] done: {} rows in {:.2}s ({:.2}ms/row avg)",
        count,
        write_elapsed.as_secs_f64(),
        write_elapsed.as_secs_f64() * 1000.0 / count as f64
    );
    match storage.explain_working_fts_top_k(&pi_memory_core::text::to_fts_query("向量检索"), 50)
    {
        Ok(plan) => println!("[fts] {count}-row-plan={plan:?}"),
        Err(error) => println!("[fts] query plan unavailable: {error}"),
    }

    // Structured term search needs its own scale fixture: it does not share the working FTS
    // index, and a working-only dataset hides its table-scan costs.
    println!("\n[structured] bulk-filling {count} rows in each structured table...");
    println!(
        "[structured] term_limit={}",
        pi_memory_core::text::structured_term_limit()
    );
    let now = pi_memory_core::runtime::now_iso();
    let facts = (0..count)
        .map(|i| FactRecord {
            fact_id: format!("bench-fact:{i}"),
            subject: "project".into(),
            predicate: "decision".into(),
            object: format!(
                "{} is used for retrieval decision {i}",
                topics[i % topics.len()]
            ),
            confidence: 0.5 + (i % 5) as f64 * 0.1,
            source_memory_id: None,
            session_id: "bench-session".into(),
            project_scope: scope.clone(),
            timestamp: now.clone(),
        })
        .collect::<Vec<_>>();
    let preferences = (0..count)
        .map(|i| PreferenceRecord {
            id: format!("bench-preference:{i}"),
            preference: format!("回复使用中文，关注 {}", topics[i % topics.len()]),
            topic: Some(topics[i % topics.len()].into()),
            context_snippet: None,
            source_memory_id: None,
            session_id: "bench-session".into(),
            project_scope: scope.clone(),
            created_at: now.clone(),
        })
        .collect::<Vec<_>>();
    let instructions = (0..count)
        .map(|i| InstructionRecord {
            id: format!("bench-instruction:{i}"),
            instruction: format!("保留 {} 的迁移决策", topics[i % topics.len()]),
            topic: Some(topics[i % topics.len()].into()),
            active: true,
            context_snippet: None,
            source_memory_id: None,
            session_id: "bench-session".into(),
            project_scope: scope.clone(),
            created_at: now.clone(),
        })
        .collect::<Vec<_>>();
    let timeline = (0..count)
        .map(|i| TimelineEventRecord {
            event_id: format!("bench-timeline:{i}"),
            date: format!("2026-08-{:02}", i % 28 + 1),
            description: format!(
                "{} 阶段记录了项目检索方案的时间线事件 {i}",
                topics[i % topics.len()]
            ),
            source: Some("bench".into()),
            source_memory_id: None,
            session_id: "bench-session".into(),
            project_scope: scope.clone(),
            created_at: now.clone(),
        })
        .collect::<Vec<_>>();
    storage.begin_tx().expect("begin structured fixture tx");
    storage
        .insert_facts(&facts)
        .expect("insert benchmark facts");
    storage
        .insert_preferences(&preferences)
        .expect("insert benchmark preferences");
    storage
        .insert_instructions(&instructions)
        .expect("insert benchmark instructions");
    storage
        .insert_timeline_events(&timeline)
        .expect("insert benchmark timeline events");
    storage.commit_tx().expect("commit structured fixture");
    println!("[structured] fixture ready");

    // Sample 5 rows through the single-turn retain path (the real per-turn behavior)
    if count >= 5 {
        println!(
            "\n[single write] sampling 5 retains (real path: rule extraction + single embed)..."
        );
        let mut single_latencies = Vec::new();
        for (i, text) in texts.iter().enumerate().take(5) {
            let t = Instant::now();
            retain_turn_memories(
                &storage,
                100_000 + i as i64,
                text,
                "bench-session",
                &scope,
                &shared,
                &mut vectors,
                None,
            )
            .expect("retain failed");
            single_latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let avg_single = single_latencies.iter().sum::<f64>() / single_latencies.len() as f64;
        println!("[single write] avg {avg_single:.2} ms/row");
    }

    // ---------- recall benchmark ----------
    let query_prompts: Vec<String> = (0..queries)
        .map(|q| {
            let topic = topics[(q * 3) % topics.len()];
            match q % 2 {
                0 => format!("{topic} 怎么实现，相关决策是什么？"),
                _ => format!("What was decided about {topic}?"),
            }
        })
        .collect();

    println!("\n[recall] running {queries} queries (full buildRecallInjection path)...");
    let mut latencies: Vec<f64> = Vec::with_capacity(queries);
    let mut fts_candidates = 0usize;
    let mut vector_candidates = 0usize;
    let mut structured_candidates = 0usize;
    let mut injection_hits = 0usize;
    let mut injected_with_fts = 0usize;
    let mut injected_with_vector = 0usize;
    let mut injected_with_structured = 0usize;
    let mut structured_latencies = Vec::with_capacity(queries);
    for prompt in &query_prompts {
        let fts_query = pi_memory_core::text::to_fts_query(prompt);
        let fts_working_count = storage
            .search_working_fts_top_k(&fts_query, &scope, 50)
            .expect("FTS candidate query failed")
            .len();
        let fts_episodic_count = storage
            .search_episodic_fts_top_k(&fts_query, &scope, 50)
            .expect("episodic FTS candidate query failed")
            .len();
        let structured_start = Instant::now();
        let structured_query = pi_memory_core::text::structured_recall_terms(prompt).join(" ");
        let structured_count = storage
            .search_facts_like(&structured_query, &scope)
            .expect("structured fact query failed")
            .len()
            + storage
                .search_preferences_like(&structured_query, &scope)
                .expect("structured preference query failed")
                .len()
            + storage
                .search_instructions_like(&structured_query, &scope)
                .expect("structured instruction query failed")
                .len()
            + storage
                .search_timeline_like(&structured_query, &scope)
                .expect("structured timeline query failed")
                .len();
        structured_latencies.push(structured_start.elapsed().as_secs_f64() * 1000.0);
        let vector_count = vectors
            .search_global_candidates(&storage, &shared, prompt, 50)
            .expect("vector candidate query failed")
            .len();
        fts_candidates += fts_working_count + fts_episodic_count;
        vector_candidates += vector_count;
        structured_candidates += structured_count;
        let t = Instant::now();
        let injection = build_recall_injection(
            &storage,
            prompt,
            &scope,
            "bench-session",
            &shared,
            &mut vectors,
        )
        .expect("recall failed");
        latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        injection_hits += injection
            .lines()
            .filter(|line| line.starts_with("- ["))
            .count();
        let diagnostics = pi_memory_core::runtime::last_recall_diagnostics(&scope);
        injected_with_fts += diagnostics.injected_with_fts;
        injected_with_vector += diagnostics.injected_with_vector;
        injected_with_structured += diagnostics.injected_with_structured;
        // Sanity-check that the injection is non-empty (when FTS hits)
        if injection.is_empty() {
            eprintln!("  [warn] no hit for query: {prompt}");
        }
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = latencies.iter().sum::<f64>() / latencies.len() as f64;
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[(latencies.len() as f64 * 0.95) as usize];
    let max = latencies[latencies.len() - 1];
    println!("[recall] latency (ms): avg {avg:.2} | P50 {p50:.2} | P95 {p95:.2} | max {max:.2}");
    println!(
        "[recall] avg candidates/query: fts {:.1} | vector {:.1} | structured {:.1} | injected {:.1}",
        fts_candidates as f64 / queries.max(1) as f64,
        vector_candidates as f64 / queries.max(1) as f64,
        structured_candidates as f64 / queries.max(1) as f64,
        injection_hits as f64 / queries.max(1) as f64,
    );
    println!(
        "[recall] injected source hits/query (overlapping): fts {:.1} | vector {:.1} | structured {:.1}",
        injected_with_fts as f64 / queries.max(1) as f64,
        injected_with_vector as f64 / queries.max(1) as f64,
        injected_with_structured as f64 / queries.max(1) as f64,
    );
    structured_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let structured_p95 = if structured_latencies.is_empty() {
        0.0
    } else {
        structured_latencies[((structured_latencies.len() as f64 * 0.95) as usize)
            .min(structured_latencies.len() - 1)]
    };
    if !structured_latencies.is_empty() {
        println!(
            "[structured] query latency (ms): P50 {:.2} | P95 {:.2} | max {:.2}",
            structured_latencies[structured_latencies.len() / 2],
            structured_p95,
            structured_latencies[structured_latencies.len() - 1],
        );
    }

    // Database file size
    let db_size = std::fs::metadata(&db_path)
        .map(|m| m.len() as f64 / 1024.0 / 1024.0)
        .unwrap_or(0.0);
    println!("[storage] db file: {:.1} MB", db_size);

    // ---------- summary (for README) ----------
    println!("\n== summary ==");
    println!("count={count}");
    println!("write_total_s={:.2}", write_elapsed.as_secs_f64());
    println!(
        "write_per_item_ms={:.2}",
        write_elapsed.as_secs_f64() * 1000.0 / count as f64
    );
    println!("recall_avg_ms={avg:.2}");
    println!("recall_p50_ms={p50:.2}");
    println!("recall_p95_ms={p95:.2}");
    println!("recall_max_ms={max:.2}");
    println!("structured_p95_ms={structured_p95:.2}");
    println!("db_size_mb={db_size:.1}");

    let mut gates_failed = false;
    if assert_gates {
        println!(
            "[gates] structured_p95_ms={structured_p95:.2}/20.00 recall_p95_ms={p95:.2}/100.00"
        );
        if structured_p95 > 20.0 {
            eprintln!("[gates] structured query P95 exceeds the 20 ms limit");
            gates_failed = true;
        }
        if p95 > 100.0 {
            eprintln!("[gates] end-to-end recall P95 exceeds the 100 ms limit");
            gates_failed = true;
        }
    }

    let _ = std::fs::remove_dir_all(&data_dir);
    if gates_failed {
        std::process::exit(1);
    }
}

/// Measure the pure 3-source RRF merge without opening a database or loading a model.
fn benchmark_recall_merge(iterations: usize) {
    let scope = "/pi-memory-rrf-benchmark";
    let make_source = |source: usize| {
        (0..50)
            .map(|rank| {
                let mut candidate = RecallCandidateInput {
                    memory_id: format!("merge-{rank:02}"),
                    memory_kind: MemoryKind::Working,
                    project_scope: scope.into(),
                    content: format!("synthetic candidate {rank}"),
                    fts_score: None,
                    embedding_score: None,
                    structured_score: None,
                };
                match source {
                    0 => candidate.fts_score = Some((50 - rank) as f64),
                    1 => candidate.embedding_score = Some((50 - rank) as f64),
                    _ => candidate.structured_score = Some((50 - rank) as f64),
                }
                candidate
            })
            .collect::<Vec<_>>()
    };
    let fts = make_source(0);
    let vectors = make_source(1);
    let structured = make_source(2);
    let mut latencies = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let merged = recall_memories(RecallMemoriesOptions {
            project_scope: scope,
            limit: 50,
            fts_candidates: &fts,
            embedding_candidates: &vectors,
            structured_candidates: &structured,
        });
        assert_eq!(merged.len(), 50, "all shared IDs should merge once");
        latencies.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    latencies.sort_by(f64::total_cmp);
    let percentile = |quantile: f64| {
        latencies[((latencies.len() as f64 * quantile).ceil() as usize)
            .saturating_sub(1)
            .min(latencies.len() - 1)]
    };
    println!(
        "[rrf-merge] candidates_per_source=50 sources=3 iterations={iterations} P50={:.3}ms P95={:.3}ms max={:.3}ms gate_p95_ms=5.000",
        percentile(0.50),
        percentile(0.95),
        latencies[latencies.len() - 1],
    );
}

fn arg(args: &[String], name: &str) -> Option<usize> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

#[allow(dead_code)]
fn _keep_pathbuf(_: PathBuf) {}
