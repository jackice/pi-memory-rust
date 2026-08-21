# pi-memory-rust

A Rust rewrite of pi-memory: a local memory extension for Pi, with storage and retrieval running entirely inside a persistent Rust subprocess.

## Architecture

```
Pi host (Node ≥ 22.19 / Bun)
└── TS shell (ts/)                Event bindings + /memory commands + LLM proxy
        │  JSON-RPC over stdio (persistent process; crash isolation + timeout kill)
        ▼
    Rust subprocess (crates/core)
    ├── turso_core                Storage engine (SQLite-compatible format + Tantivy FTS)
    ├── turbovec                  Vector ANN (quantized + SIMD + allowlist rerank)
    └── ort                       ONNX embedding (all-MiniLM, 384-dim)
```

Design decisions (see `docs/superpowers/specs/` for the full design):

- **Subprocess JSON-RPC instead of NAPI**: crash isolation (a Rust crash never takes down the Pi host), timeout-killable calls, minimal tooling, host-version independent. NAPI's zero-copy advantage is irrelevant at this call frequency.
- **Heavy work stays in the subprocess**: only small payloads cross the process boundary (prompt, Top-K candidates); embedding batches never leave the Rust process.
- **Behavior-equivalent**: recall/retain/consolidate are bit-exact with the TS reference implementation (pi-memory v0.1.2).

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

- Project A's memories never appear in project B's recall (cross-db + scope filtering).
- Deleting a project = deleting its directory; a corrupted db never affects other projects.
- The embedding model is loaded once globally (~90MB, not per project); vector indexes are isolated per project.

## Status

- [x] M1-T1 Scaffold: cargo workspace, JSON-RPC protocol (version handshake), TS client (spawn/timeout/restart)
- [x] M1-T2 Storage migration (turso_core + Tantivy FTS)
- [x] M1-T3 Rule-path migration (recall / extraction / consolidation, behavior-equivalent)
- [x] M1-T4 Bridge + /memory commands
- [x] M2 Semantic embedding + turbovec index (hybrid recall, FTS blind-spot hit in 0.01s)
- [x] M3 LLM proxy (one-way llmResult RPC) + migration tool + multi-platform leaf packages

## Entry points

| Entry | Description |
|---|---|
| `/memory view\|stats\|diagnose\|clear\|rebuild` | Memory maintenance |
| `/memory migrate <path>` | Migrate a legacy pi-memory (TS) SQLite database |
| `/memory prepare-model` | Manually trigger embedding model download |
| `PI_MEMORY_MODEL_PATH` | Model directory (default `~/.cache/pi-memory-rust/model`) |
| `PI_MEMORY_RUST_BINARY` | Explicit path to the Rust binary |
| `PI_MEMORY_DATA_DIR` | Data directory (default `~/.local/share/pi-memory-rust`) |

## Model

all-MiniLM-L6-v2 (384-dim, ~90MB ONNX): auto-downloaded in the background on first start (HF official + hf-mirror fallback); degrades to FTS-only when missing. Manual trigger: `/memory prepare-model`.
