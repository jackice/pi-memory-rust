//! pi-memory core library.
//!
//! Pure Rust implementation: storage (turso_core), rule-based extraction/consolidation, hybrid recall.
//! Behavior is bit-exact with the TS reference implementation (pi-memory v0.1.2).

pub mod core;
pub mod embedding;
pub mod embedding_runtime;
pub mod llm;
pub mod model_download;
pub mod models;
pub mod paths;
pub mod protocol;
pub mod runtime;
pub mod storage;
pub mod vector_index;

/// Info-level diagnostic log: only written to stderr when PI_MEMORY_DEBUG=1 is set.
/// Child stderr is relayed by the host, so unconditional output would corrupt the pi TUI;
/// error details are already returned via RPC error responses, stderr is extra diagnostics only.
pub fn debug_log(msg: &str) {
    if std::env::var("PI_MEMORY_DEBUG").is_ok() {
        eprintln!("{msg}");
    }
}
