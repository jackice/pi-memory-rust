//! Storage layer: persistence backed by turso_core (SQLite-compatible).
//!
//! Design notes:
//! - the `Storage` trait isolates the engine (currently turso_core; keeps a future rusqlite switch feasible);
//! - table schemas are ported from the TS schema.ts, keeping fields and semantics aligned;
//! - FTS uses Turso Tantivy syntax (`CREATE INDEX ... USING fts`).

pub mod turso;

pub use turso::TursoStorage;

/// Storage engine abstraction: isolates the turso_core API for testing and future replacement.
pub trait Storage: Send + Sync {
    /// Initialize the schema (idempotent).
    fn init(&mut self) -> anyhow::Result<()>;
    /// Health check: returns diagnostics such as the database path.
    fn diagnose(&self) -> anyhow::Result<serde_json::Value>;
}

