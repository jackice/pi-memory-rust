# pi-memory-rust

A Rust rewrite of pi-memory: a local memory extension for Pi, with storage and retrieval running entirely inside a persistent Rust subprocess.

## Architecture

```
Pi host (Node ≥ 22.19 / Bun)
└── TS shell (ts/)                Event bindings, /memory commands, LLM proxy
        │  JSON-RPC over socket or stdio
        ▼
    Rust server (crates/core)
    ├── turso_core                Per-project database + FTS
    ├── turbovec                  Per-project vector index
    └── ort                       Shared ONNX embedding model
```

Design decisions (see `docs/superpowers/specs/` for the full design):

- **JSON-RPC instead of NAPI**: the Rust service is isolated from the Pi host and can be shared by sessions over a local socket; stdio remains available as a fallback.
- **Heavy work stays in the subprocess**: only small payloads cross the process boundary (prompt, Top-K candidates); embedding batches never leave the Rust process.
- **Documented retrieval semantics**: FTS, vector, and structured results are retrieved independently and merged with RRF. Ranking is intentionally not bit-exact with the older TypeScript implementation.
- **Durable turn processing**: turn-end writes to a persistent outbox; extraction runs asynchronously and retries after restart.
- **Automatic consolidation**: sessions consolidate at a configurable threshold and around compaction; set `PI_MEMORY_AUTO_CONSOLIDATE=0` to disable it.
- Completed outbox rows are pruned after 30 days; exhausted failures are retained for 90 days for diagnosis, while retryable work is kept.
- RPC timeouts stop waiting for that response; they do not cancel server work. Writes use idempotent IDs/leases, so retrying the same turn is safe, but a timed-out request may still finish. A single timeout leaves the shared socket intact; three consecutive socket timeouts trigger reconnect.

## Development

```bash
# Build the Rust service
cargo build --release
cp target/release/pi-memory-server ts/bin/

# Rust tests
cargo test

# TS smoke tests (process lifecycle + JSON-RPC roundtrip)
bun test ts/test/
```

## Data Isolation

**Physical isolation per project (cwd)**: each project gets its own database file (same layout semantics as the TS version).

```
<data_dir>/memories/<base64url(projectScope)>/memory.db
# data_dir defaults to ~/.local/share/pi-memory-rust (override with PI_MEMORY_DATA_DIR)
```

- New memories are isolated by project database and scope. Legacy rows with an empty `project_scope` remain visible for compatibility inside the database where they were imported; review old shared databases before relying on strict project isolation.
- Deleting a project = deleting its directory; a corrupted db never affects other projects.
- The embedding model is loaded once globally (~90MB, not per project); vector indexes are isolated per project and are hydrated from the database on first open.
- On Unix, the data/database directories are private (`0700`), the main database file and socket are restricted (`0600`), and debug logs omit memory bodies. SQLite sidecar files live inside the private database directory.
- The local socket server uses a lock file to serialize cold starts. Windows uses stdio because named-pipe support is not implemented.
- The default data directory is private to the current OS account. On shared machines, use separate OS accounts or separate `PI_MEMORY_DATA_DIR` paths for users who must not share memory databases; back up and remove that directory when retiring the account or its stored memories.
- **Single-session stability is the default**: databases open in WAL mode with a 3s busy timeout
  (harmless for one process). A second pi session touching the same project DB fails fast with a
  clear "locked by another process" error and degrades gracefully.
- **Experimental multi-session sharing (opt-in)**: set `PI_MEMORY_MULTIPROCESS=1` to lift the
  exclusive file lock so multiple sessions can share one project DB. This uses libSQL's experimental
  multiprocess WAL, which can panic on very large transactions ("shared WAL frame ids must increase
  monotonically") — enable only if you accept that risk.

## Status

- [x] M1-T1 Scaffold: cargo workspace, JSON-RPC protocol (version handshake), TS client (spawn/timeout/restart)
- [x] M1-T2 Storage migration (turso_core + Tantivy FTS)
- [x] M1-T3 Rule-path migration (recall / extraction / consolidation, semantics documented)
- [x] M1-T4 Bridge + /memory commands
- [x] M2 Semantic embedding + turbovec index (independent vector retrieval and hybrid ranking)
- [x] M3 LLM proxy (one-way llmResult RPC) + migration tool + platform-specific leaf package generation (npm 0.1.17: macOS arm64)

## Entry points

| Entry | Description |
|---|---|
| `/memory view\|stats\|diagnose\|clear\|purge\|rebuild\|rebuild-embeddings\|consolidate` | Memory maintenance |
| `/memory migrate <path>` | Import the current project scope from a legacy SQLite database |
| `/memory prepare-model` | Manually trigger embedding model download |
| `/memory job <id>` | Resume polling a maintenance job that outlived the command's 30-minute wait window |
| `PI_MEMORY_EMBEDDING_MODEL` | Active model ID (default `all-MiniLM-L6-v2`) |
| `PI_MEMORY_MODEL_PATH` | Model directory (default `~/.cache/pi-memory-rust/model`; custom models require a manifest) |
| `PI_MEMORY_MODEL_MANIFEST` | Absolute path to the manifest for a custom model directory |
| `PI_MEMORY_MODEL_VERIFY=0` | Skip model file hashes only in debug/development; release builds reject this setting |
| `PI_MEMORY_RUST_BINARY` | Explicit path to the Rust binary |
| `PI_MEMORY_DATA_DIR` | Data directory (default `~/.local/share/pi-memory-rust`) |
| `PI_MEMORY_AUTO_CONSOLIDATE=0` | Disable threshold/compaction/shutdown consolidation |
| `PI_MEMORY_CONSOLIDATE_EVERY` | Working turns before automatic consolidation (default `20`) |
| `PI_MEMORY_CONSOLIDATE_MAX_INPUT_BYTES` | Maximum UTF-8 prompt size for consolidation LLM calls (default `65536`, clamped to `1024–1048576`; oversized batches use the full rule summary) |
| `PI_MEMORY_RECALL_TOKEN_BUDGET` | Approximate recall injection budget (default `1000`, capped at `1200`) |
| `PI_MEMORY_FTS_TOP_K` / `PI_MEMORY_VECTOR_TOP_K` / `PI_MEMORY_STRUCTURED_TOP_K` | Candidate limits per recall source (defaults `50` / `50` / `30`) |
| `PI_MEMORY_STRUCTURED_TERM_LIMIT` | Terms used per structured LIKE query (default `2`, configurable from `1` to `3`) |
| `PI_MEMORY_RRF_K` | Reciprocal-rank smoothing constant (default `60`) |
| `PI_MEMORY_FTS_WEIGHT` / `PI_MEMORY_VECTOR_WEIGHT` / `PI_MEMORY_STRUCTURED_WEIGHT` | Relative RRF source weights (defaults `1` / `1` / `0.8`) |
| `PI_MEMORY_RECALL_MIN_SCORE` / `PI_MEMORY_RECALL_RELATIVE_CUTOFF` | Relevance cutoffs for fused recall results (defaults `0.012` / `0.2`) |
| `PI_MEMORY_RECALL_MODE=legacy` | One-release rollback for the older recall ranking and vector allowlist path |
| `PI_MEMORY_FTS_MODE=content` | Query the retained v1 `content` FTS index during rollback |
| `PI_MEMORY_LEGACY_CONSOLIDATION=1` | Temporarily allow old consolidation RPC semantics; `summary_of` may not exactly match the summary input |
| `PI_MEMORY_MAX_OPEN_PROJECTS` | Soft limit for idle in-memory project runtimes (default `16`, range `1–256`; only unshared runtimes are evicted, so the count can temporarily exceed the limit while projects are in use) |
| `PI_MEMORY_FTS_MIGRATION_LEASE_MS` | FTS migration leader lease (default `30000`, clamped to `3000–600000`; renewed during each backfill batch and before state changes) |

`/memory clear` clears the current project. `/memory clear <project-scope>` targets another project database. Clearing every stored project requires the explicit command `/memory clear all confirm`; it reports each scope independently because cross-project deletion is not one atomic transaction.

`/memory diagnose` includes the active embedding model and dimension, the most recent recall counts, injected-memory source overlap (FTS/vector/structured), and up to 100 dropped candidate records (`memoryId`, kind, fused score, source ranks, and reason such as threshold, result limit, consolidated, duplicate, kind quota, topic quota, or token budget). Recall injection also limits repeated near-identical topics to two memories. It reports pending consolidation work, this process's failure count, last attempt, next retry, budget skips, and a bounded error message. It omits memory text.

## Model

all-MiniLM-L6-v2 (384-dim, ~90MB ONNX) is downloaded in the background and checked against the pinned model manifest. Download/load failures retry in the background with capped exponential backoff; recall falls back to FTS while the model is unavailable. Custom ONNX sentence encoders can specify arbitrary model/tokenizer filenames and extra required weight files, use two inputs (`input_ids`, `attention_mask`) or three inputs (plus `token_type_ids`), dynamic output dimensions, mean or CLS pooling, and optional query/document prefixes. Set `PI_MEMORY_EMBEDDING_MODEL`, `PI_MEMORY_MODEL_PATH`, and an absolute `PI_MEMORY_MODEL_MANIFEST` before starting the server. After switching, run `/memory rebuild-embeddings`; vectors are stored per model ID, so switching back and rebuilding the old model does not require re-embedding memories that still have that model's vector. Use `/memory prepare-model` to request model preparation.

To compare local candidate models on the same bilingual fixture, run `eval/compare-models.sh <id> <model-dir> <manifest.json> [...]`. It creates a separate temporary database per model and reports ranking quality, model size, cold start, and recall latency. The included synthetic judgments are only an evaluator smoke set; do not use them as the model-selection gate described in the repair plan.

`prepare-model`, `migrate`, `rebuild`, `rebuild-embeddings`, `clear`, and `purge` run as server jobs. The command polls job status instead of holding a long RPC; if the command wait expires, use `/memory job <id>` to resume polling while the server is still running. `clear all confirm` retains an explicit confirmation requirement and reports each project scope in the job details.

## Benchmark

Run the scale benchmark with `cargo run --release -p pi-memory-core --bin pi-memory-bench` (default 10k memories and 100 bilingual queries). It reports write/recall latency, average FTS/vector/structured candidate counts, injected-memory counts, and overlapping per-source hit counts for the memories that survived recall filtering. Historical numbers from earlier retrieval paths are omitted because they do not measure the current hybrid implementation.

Before changing the default structured recall path, run `cargo run --release -p pi-memory-core --bin pi-memory-bench -- --assert-gates`. This requires at least 10k rows per structured table and exits nonzero if structured-query P95 exceeds 20 ms or end-to-end recall P95 exceeds 100 ms.

Measure only the RRF merge path, without a database or model, with `cargo run --release -p pi-memory-core --bin pi-memory-bench -- --merge-only --iterations 1000`. It merges three 50-candidate lists with shared IDs and reports P50/P95/max against the 5 ms P95 gate.

Latest gated synthetic scale run (2026-11-23, macOS arm64, Rust 1.97.1, release build; 10k working memories and 10k rows in each of facts, preferences, instructions, and timeline; 100 bilingual queries; local model ready): `--assert-gates` passed; the exact multi-column working-memory FTS query plan was `QUERY INDEX METHOD fts`; structured query P50/P95 was 13.89/15.29 ms; end-to-end recall P50/P95 was 16.93/18.18 ms; bulk write averaged 1.60 ms per working row. The benchmark fixture is synthetic, and these figures are a local reference rather than a hardware-independent guarantee. The structured path defaults to two informative terms (configurable up to three) to stay below its 20 ms P95 gate; this run averaged 72 structured candidates and 2.9 injected memories per query.

For labeled ranking metrics, see [`eval/README.md`](eval/README.md) and run `pi-memory-eval` against a dedicated database. The included synthetic fixture is for repeatable plumbing checks; use manually labeled project memories for quality decisions.
