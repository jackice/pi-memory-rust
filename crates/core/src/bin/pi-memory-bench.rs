//! 万级记忆基准测试工具。
//!
//! 用法：`cargo run --release -p pi-memory-core --bin pi-memory-bench [--count 10000] [--queries 100]`
//!
//! 测什么（全部本地计算，零 LLM API 消耗）：
//! - 写入：N 条记忆（retain 真实路径：规则提取 + ONNX embedding + turbovec 索引 + Tantivy FTS）
//! - 召回：M 个查询的 buildRecallInjection 延迟（FTS 粗筛 + embedding 精排 + 多路合并）
//! 验收对照：万级记忆 recall 注入 ≤ 300ms（spec §6）。

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

    // 隔离的数据目录（不污染真实数据）
    let data_dir = std::env::temp_dir().join("pi-memory-bench");
    let _ = std::fs::remove_dir_all(&data_dir);
    let db_path = paths::ensure_project_db_dir(&data_dir, &scope).expect("创建基准库失败");
    let storage = TursoStorage::open(db_path.to_str().expect("路径 UTF-8")).expect("打开基准库失败");
    let shared = SharedEmbedder::new();

    println!("== pi-memory-rust 基准测试 ==");
    println!("记忆条数: {count} | 查询数: {queries}");
    println!("模型: {}", shared.status());
    if !shared.ensure_loaded() {
        eprintln!("[bench] 模型未就绪（embeddings 将降级，结果仅供 FTS 参考）。可先运行 /memory prepare-model 或设置 PI_MEMORY_MODEL_PATH");
    }

    // 构造测试文本：模板化中文/英文混合，保证 FTS 与 embedding 都有内容
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

    // ---------- 写入基准 ----------
    // 批量填充：批量 embed（onnxruntime 批量推理效率高）+ 逐条写入/索引，
    // 模拟系统在积累到万级记忆后的状态；单条 retain 路径在 [单条写入] 节单独测。
    let mut vectors = ProjectVectors::new().expect("创建向量索引失败");
    println!("\n[写入] 批量填充 {count} 条（embed 批大小 64，事务提交）...");
    let write_start = Instant::now();
    let mut embed_total = std::time::Duration::new(0, 0);
    let batch_size = 64usize;
    storage.begin_tx().expect("开启事务失败");
    storage.drop_fts().expect("删除 FTS 索引失败（填充阶段）");
    let mut all_items: Vec<(pi_memory_core::models::MemoryKind, String, String)> = Vec::with_capacity(count);
    let mut all_embeddings: Vec<Vec<f32>> = Vec::with_capacity(count);
    let mut records: Vec<pi_memory_core::models::WorkingMemoryRecord> = Vec::with_capacity(count);
    for (batch_start, chunk) in texts.chunks(batch_size).enumerate() {
        let t_embed = Instant::now();
        let embeddings = shared.embed(&chunk.to_vec()).expect("embed 失败");
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
            println!("  ... {} 条（{:.1}s）", (batch_start + 1) * batch_size, write_start.elapsed().as_secs_f64());
        }
    }
    let t_db = Instant::now();
    for record in &records {
        storage.insert_working(record).expect("insert 失败");
    }
    let db_elapsed = t_db.elapsed();
    let t_vec = Instant::now();
    vectors
        .save_embeddings_batch(&storage, &all_items, &all_embeddings)
        .expect("批量向量入库失败");
    let vec_elapsed = t_vec.elapsed();
    storage.commit_tx().expect("提交事务失败");
    let t_fts = Instant::now();
    storage.rebuild_fts().expect("重建 FTS 索引失败");
    let fts_elapsed = t_fts.elapsed();
    let write_elapsed = write_start.elapsed();
    println!(
        "  [分阶段] embed {:.1}s | DB写入 {:.1}s | 向量索引 {:.1}s | FTS重建 {:.1}s",
        embed_total.as_secs_f64(),
        db_elapsed.as_secs_f64(),
        vec_elapsed.as_secs_f64(),
        fts_elapsed.as_secs_f64()
    );
    println!(
        "[写入] 完成：{} 条，耗时 {:.2}s（平均 {:.2}ms/条）",
        count,
        write_elapsed.as_secs_f64(),
        write_elapsed.as_secs_f64() * 1000.0 / count as f64
    );

    // 单条 retain 路径（真实用户每轮行为）抽样 5 条
    if count >= 5 {
        println!("\n[单条写入] 抽样 5 条 retain（真实路径，含规则提取 + 单条 embed）...");
        let mut single_latencies = Vec::new();
        for i in 0..5 {
            let t = Instant::now();
            retain_turn_memories(
                &storage, 100_000 + i as i64, &texts[i], "bench-session", &scope, &shared, &mut vectors, None,
            )
            .expect("retain 失败");
            single_latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        let avg_single = single_latencies.iter().sum::<f64>() / single_latencies.len() as f64;
        println!("[单条写入] 平均 {avg_single:.2} ms/条");
    }

    // ---------- 召回基准 ----------
    let query_prompts: Vec<String> = (0..queries)
        .map(|q| format!("{} 方案怎么样？", topics[(q * 3) % topics.len()]))
        .collect();

    println!("\n[召回] 开始（{queries} 个查询，buildRecallInjection 全链路）...");
    let mut latencies: Vec<f64> = Vec::with_capacity(queries);
    for prompt in &query_prompts {
        let t = Instant::now();
        let injection = build_recall_injection(&storage, prompt, &scope, "bench-session", &shared, &mut vectors)
            .expect("recall 失败");
        latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        // 校验注入非空（FTS 命中的话）
        if injection.is_empty() {
            eprintln!("  [warn] 查询无命中: {prompt}");
        }
    }
    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg = latencies.iter().sum::<f64>() / latencies.len() as f64;
    let p50 = latencies[latencies.len() / 2];
    let p95 = latencies[(latencies.len() as f64 * 0.95) as usize];
    let max = latencies[latencies.len() - 1];
    println!("[召回] 延迟（ms）: 平均 {avg:.2} | P50 {p50:.2} | P95 {p95:.2} | 最大 {max:.2}");

    // 库文件大小
    let db_size = std::fs::metadata(&db_path).map(|m| m.len() as f64 / 1024.0 / 1024.0).unwrap_or(0.0);
    println!("[存储] 库文件: {:.1} MB", db_size);

    // ---------- 汇总（README 用） ----------
    println!("\n== 汇总 ==");
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
