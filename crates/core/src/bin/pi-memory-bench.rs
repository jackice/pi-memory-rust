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

use pi_memory_core::embedding_runtime::{ProjectVectors, SharedEmbedder};
use pi_memory_core::paths;
use pi_memory_core::runtime::{build_recall_injection, retain_turn_memories};
use pi_memory_core::storage::TursoStorage;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let count: usize = arg(&args, "--count").unwrap_or(10_000);
    let queries: usize = arg(&args, "--queries").unwrap_or(100);
    let scope = format!("/tmp/pi-memory-bench-{}", std::process::id());

    // Isolated data directory (does not touch real data)
    let data_dir = std::env::temp_dir().join("pi-memory-bench");
    let _ = std::fs::remove_dir_all(&data_dir);
    let db_path = paths::ensure_project_db_dir(&data_dir, &scope).expect("failed to create benchmark db");
    let storage = TursoStorage::open(db_path.to_str().expect("path UTF-8")).expect("failed to open benchmark db");
    let shared = SharedEmbedder::new();

    println!("== pi-memory-rust benchmark ==");
    println!("memories: {count} | queries: {queries}");
    println!("model: {}", shared.status());
    if !shared.ensure_loaded() {
        eprintln!("[bench] model not ready (embeddings will degrade; results are FTS-only). Run /memory prepare-model or set PI_MEMORY_MODEL_PATH");
    }

    // Build test texts: templated CJK/English mix so both FTS and embedding have content
    let topics = ["SQLite 迁移", "Rust 重写", "向量检索", "全文搜索", "会话整合",
                  "记忆隔离", "embedding 模型", "Tantivy 索引", "turbovec 量化", "JSON-RPC 桥接"];
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
    storage.drop_fts().expect("failed to drop FTS index (fill phase)");
    let mut all_items: Vec<(pi_memory_core::models::MemoryKind, String, String)> = Vec::with_capacity(count);
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(count);
    let mut records: Vec<pi_memory_core::models::WorkingMemoryRecord> = Vec::with_capacity(count);
    for (batch_start, chunk) in texts.chunks(batch_size).enumerate() {
        let t_embed = Instant::now();
        let embeddings = shared.embed(chunk).expect("embed failed");
        embed_total += t_embed.elapsed();
        for (offset, (text, embedding)) in chunk.iter().zip(embeddings.iter()).enumerate() {
            let i = batch_start * batch_size + offset;
            let memory_id = format!("bench:{i}");
            all_items.push((pi_memory_core::models::MemoryKind::Working, memory_id.clone(), text.clone()));
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
            println!("  ... {} rows ({:.1}s)", (batch_start + 1) * batch_size, write_start.elapsed().as_secs_f64());
        }
    }
    let t_db = Instant::now();
    for record in &records {
        storage.insert_working(record).expect("insert failed");
    }
    let db_elapsed = t_db.elapsed();
    let t_vec = Instant::now();
    vectors
        .save_embeddings_batch(&storage, &all_items, &all_embeddings)
        .expect("bulk vector persist failed");
    let vec_elapsed = t_vec.elapsed();
    storage.commit_tx().expect("failed to commit tx");
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

    // Sample 5 rows through the single-turn retain path (the real per-turn behavior)
    if count >= 5 {
        println!("\n[single write] sampling 5 retains (real path: rule extraction + single embed)...");
        let mut single_latencies = Vec::new();
        for (i, text) in texts.iter().enumerate().take(5) {
            let t = Instant::now();
            retain_turn_memories(
                &storage, 100_000 + i as i64, text, "bench-session", &scope, &shared, &mut vectors, None,
            )
            .expect("retain failed");
            single_latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let avg_single = single_latencies.iter().sum::<f64>() / single_latencies.len() as f64;
        println!("[single write] avg {avg_single:.2} ms/row");
    }

    // ---------- recall benchmark ----------
    let query_prompts: Vec<String> = (0..queries)
        .map(|q| format!("how about {}?", topics[(q * 3) % topics.len()]))
        .collect();

    println!("\n[recall] running {queries} queries (full buildRecallInjection path)...");
    let mut latencies: Vec<f64> = Vec::with_capacity(queries);
    for prompt in &query_prompts {
        let t = Instant::now();
        let injection = build_recall_injection(&storage, prompt, &scope, "bench-session", &shared, &mut vectors)
            .expect("recall failed");
        latencies.push(t.elapsed().as_secs_f64() * 1000.0);
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

    // Database file size
    let db_size = std::fs::metadata(&db_path).map(|m| m.len() as f64 / 1024.0 / 1024.0).unwrap_or(0.0);
    println!("[storage] db file: {:.1} MB", db_size);

    // ---------- summary (for README) ----------
    println!("\n== summary ==");
    println!("count={count}");
    println!("write_total_s={:.2}", write_elapsed.as_secs_f64());
    println!("write_per_item_ms={:.2}", write_elapsed.as_secs_f64() * 1000.0 / count as f64);
    println!("recall_avg_ms={avg:.2}");
    println!("recall_p50_ms={p50:.2}");
    println!("recall_p95_ms={p95:.2}");
    println!("recall_max_ms={max:.2}");
    println!("db_size_mb={db_size:.1}");

    let _ = std::fs::remove_dir_all(&data_dir);
}

fn arg(args: &[String], name: &str) -> Option<usize> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok())
}

#[allow(dead_code)]
fn _keep_pathbuf(_: PathBuf) {}

