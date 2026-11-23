//! turso_core file storage implementation (M1-T2).
//!
//! Table schemas are ported from the TS `schema.ts`; CRUD semantics match the TS `store.ts`
//! (upsert via ON CONFLICT DO UPDATE). FTS uses Turso Tantivy syntax:
//! `CREATE INDEX ... USING fts` attaches to existing tables and is maintained automatically (inserts are covered by tests;
//! the update sync behavior is covered by `tests/turso_storage.rs::turso_fts_update_sync`).

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use turso_core::types::{Text, Value};
use turso_core::{Connection, Database, DatabaseOpts, Numeric, OpenFlags, Row, Statement};

use crate::models::{
    EpisodicMemoryRecord, FactRecord, InstructionRecord, MemoryEmbeddingRecord, MemoryKind,
    PendingTurn, PreferenceRecord, SessionStateRecord, TimelineEventRecord, WorkingClaim,
    WorkingMemoryRecord,
};

use super::Storage;

/// Table schema SQL (kept aligned with the TS schema.ts).
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS working_memory (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    search_text TEXT,
    embed_text TEXT,
    source TEXT,
    session_id TEXT NOT NULL,
    project_scope TEXT NOT NULL,
    importance REAL DEFAULT 0.5,
    memory_type TEXT DEFAULT 'unknown',
    metadata_json TEXT DEFAULT '{}',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
    consolidated_at TEXT,
    consolidation_lease_owner TEXT,
    consolidation_lease_expires_at TEXT,
    consolidation_claimed_at TEXT,
    valid_until TEXT,
    superseded_by TEXT
);
CREATE TABLE IF NOT EXISTS episodic_memory (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    search_text TEXT,
    summary_of TEXT DEFAULT '',
    session_id TEXT NOT NULL,
    project_scope TEXT NOT NULL,
    importance REAL DEFAULT 0.5,
    tier INTEGER DEFAULT 1,
    metadata_json TEXT DEFAULT '{}',
    memory_type TEXT DEFAULT 'unknown',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP,
    degraded_at TEXT,
    valid_until TEXT,
    superseded_by TEXT
);
CREATE TABLE IF NOT EXISTS facts (
    fact_id TEXT PRIMARY KEY,
    subject TEXT NOT NULL,
    predicate TEXT NOT NULL,
    object TEXT NOT NULL,
    confidence REAL DEFAULT 0.5,
    source_memory_id TEXT,
    session_id TEXT NOT NULL,
    project_scope TEXT NOT NULL DEFAULT '',
    timestamp TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS preferences (
    id TEXT PRIMARY KEY,
    preference TEXT NOT NULL,
    topic TEXT,
    context_snippet TEXT,
    source_memory_id TEXT,
    session_id TEXT NOT NULL,
    project_scope TEXT NOT NULL DEFAULT '',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS instructions (
    id TEXT PRIMARY KEY,
    instruction TEXT NOT NULL,
    topic TEXT,
    active INTEGER DEFAULT 1,
    context_snippet TEXT,
    source_memory_id TEXT,
    session_id TEXT NOT NULL,
    project_scope TEXT NOT NULL DEFAULT '',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS timeline (
    event_id TEXT PRIMARY KEY,
    date TEXT NOT NULL,
    description TEXT NOT NULL,
    source TEXT,
    source_memory_id TEXT,
    session_id TEXT NOT NULL,
    project_scope TEXT NOT NULL DEFAULT '',
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS memory_embeddings (
    memory_kind TEXT NOT NULL,
    memory_id TEXT NOT NULL,
    embedding_json TEXT NOT NULL,
    model TEXT NOT NULL,
    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (memory_kind, memory_id, model)
);
CREATE TABLE IF NOT EXISTS memory_scopes (
    memory_table TEXT NOT NULL,
    project_scope TEXT NOT NULL,
    PRIMARY KEY (memory_table, project_scope)
);
CREATE TRIGGER IF NOT EXISTS working_scope_registry_insert
AFTER INSERT ON working_memory
BEGIN
    INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
    VALUES ('working_memory', NEW.project_scope);
END;
CREATE TRIGGER IF NOT EXISTS working_scope_registry_update
AFTER UPDATE OF project_scope ON working_memory
WHEN OLD.project_scope IS NOT NEW.project_scope
BEGIN
    INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
    VALUES ('working_memory', NEW.project_scope);
    DELETE FROM memory_scopes
    WHERE memory_table = 'working_memory' AND project_scope = OLD.project_scope
      AND NOT EXISTS (
          SELECT 1 FROM working_memory WHERE project_scope = OLD.project_scope
      );
END;
CREATE TRIGGER IF NOT EXISTS working_scope_registry_delete
AFTER DELETE ON working_memory
BEGIN
    DELETE FROM memory_scopes
    WHERE memory_table = 'working_memory' AND project_scope = OLD.project_scope
      AND NOT EXISTS (
          SELECT 1 FROM working_memory WHERE project_scope = OLD.project_scope
      );
END;
CREATE TRIGGER IF NOT EXISTS episodic_scope_registry_insert
AFTER INSERT ON episodic_memory
BEGIN
    INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
    VALUES ('episodic_memory', NEW.project_scope);
END;
CREATE TRIGGER IF NOT EXISTS episodic_scope_registry_delete
AFTER DELETE ON episodic_memory
BEGIN
    DELETE FROM memory_scopes
    WHERE memory_table = 'episodic_memory' AND project_scope = OLD.project_scope
      AND NOT EXISTS (
          SELECT 1 FROM episodic_memory WHERE project_scope = OLD.project_scope
      );
END;
CREATE TRIGGER IF NOT EXISTS episodic_scope_registry_update
AFTER UPDATE OF project_scope ON episodic_memory
WHEN OLD.project_scope IS NOT NEW.project_scope
BEGIN
    INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
    VALUES ('episodic_memory', NEW.project_scope);
    DELETE FROM memory_scopes
    WHERE memory_table = 'episodic_memory' AND project_scope = OLD.project_scope
      AND NOT EXISTS (
          SELECT 1 FROM episodic_memory WHERE project_scope = OLD.project_scope
      );
END;
CREATE TABLE IF NOT EXISTS session_state (
    session_id TEXT PRIMARY KEY,
    project_scope TEXT NOT NULL,
    last_retained_user_turn INTEGER NOT NULL DEFAULT 0,
    has_recalled_for_first_turn INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS app_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS fts_migration_lock (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    owner TEXT,
    expires_at TEXT,
    acquired_at TEXT,
    fencing_token INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS pending_turns (
    session_id TEXT NOT NULL,
    turn_index INTEGER NOT NULL,
    project_scope TEXT NOT NULL,
    text TEXT NOT NULL,
    prompt_missing INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'pending',
    lease_owner TEXT,
    lease_expires_at TEXT,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(session_id, turn_index)
);
CREATE INDEX IF NOT EXISTS idx_pending_turns_ready
    ON pending_turns(session_id, status, next_attempt_at, turn_index);
CREATE INDEX IF NOT EXISTS idx_pending_turns_scope_ready
    ON pending_turns(session_id, project_scope, status, next_attempt_at, turn_index);
"#;

/// turso_core file storage.
pub struct TursoStorage {
    conn: Arc<Connection>,
    path: String,
    embedding_dim: usize,
    /// 缓存 prepare 的查询语句（单线程 RPC 主循环访问；Mutex 兜底并发测试）。
    /// 复用前必须 reset + clear_bindings，避免绑定残留。
    stmts: Mutex<std::collections::HashMap<String, Statement>>,
}

impl TursoStorage {
    /// Open (or create) a local database file and initialize the schema.
    ///
    /// Concurrency notes:
    /// - multiprocess WAL lifts the exclusive file lock so multiple pi sessions can share one
    ///   project DB; combined with PRAGMA busy_timeout, cross-process write contention becomes a
    ///   brief wait instead of an immediate failure;
    /// - two processes opening a brand-new DB at the exact same moment race to create the shared
    ///   WAL coordination file; the loser gets a transient "magic mismatch" error, so retry.
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let storage = Self::open_deferred(path)?;
        if let Err(error) = storage.migrate_search_text_if_needed() {
            crate::debug_log(&format!(
                "search_text FTS migration deferred; continuing with current index: {error}"
            ));
        }
        Ok(storage)
    }

    /// Open storage without waiting for the FTS backfill. The resident server uses this path
    /// so project startup can serve against the current index while migration runs in a worker.
    pub fn open_deferred(path: &str) -> anyhow::Result<Self> {
        let mut last_err = None;
        for attempt in 0..3 {
            match Self::open_once(path) {
                Ok(storage) => return Ok(storage),
                Err(e) => {
                    last_err = Some(e);
                    if attempt < 2 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            }
        }
        Err(last_err.expect("loop always runs at least once"))
    }

    fn open_readonly(path: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            std::path::Path::new(path).is_file(),
            "migration source does not exist: {path}"
        );
        let io = Database::io_for_path(path)?;
        let opts = DatabaseOpts::new().with_index_method(true);
        let db = Database::open_file_with_flags(io, path, OpenFlags::ReadOnly, opts, None)?;
        Ok(Self {
            conn: db.connect()?,
            path: path.to_string(),
            embedding_dim: crate::vector_index::DEFAULT_DIM,
            stmts: Mutex::new(std::collections::HashMap::new()),
        })
    }

    fn open_once(path: &str) -> anyhow::Result<Self> {
        let enable_multiprocess_wal = std::env::var("PI_MEMORY_MULTIPROCESS").is_ok();
        Self::open_once_with_multiprocess_wal(path, enable_multiprocess_wal)
    }

    fn open_once_with_multiprocess_wal(
        path: &str,
        enable_multiprocess_wal: bool,
    ) -> anyhow::Result<Self> {
        let io = Database::io_for_path(path)?;
        // The Tantivy FTS index (USING fts) is an experimental index method and must be enabled explicitly.
        // libSQL's experimental multiprocess WAL (which lifts the exclusive file lock so multiple pi
        // sessions can share one project DB) is OFF by default: it can panic on large transactions
        // ("shared WAL frame ids must increase monotonically"). Enable explicitly with
        // PI_MEMORY_MULTIPROCESS=1 and accept the experimental risk; busy_timeout applies in both modes.
        let opts = DatabaseOpts::new()
            .with_index_method(true)
            .with_multiprocess_wal(enable_multiprocess_wal);
        let db = Database::open_file_with_flags(io, path, OpenFlags::default(), opts, None)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        let conn = db.connect()?;
        let embedding_dim = crate::model_manifest::configured_dimension()?;
        let storage = Self {
            conn,
            path: path.to_string(),
            embedding_dim,
            stmts: Mutex::new(std::collections::HashMap::new()),
        };
        storage.conn.execute("PRAGMA busy_timeout = 3000")?;
        storage.init()?;
        Ok(storage)
    }

    /// Initialize the schema (idempotent).
    fn init(&self) -> anyhow::Result<()> {
        self.migrate_memory_scope_registry()?;
        self.conn.prepare_execute_batch(SCHEMA_SQL)?;
        self.ensure_scope_registry_update_triggers()?;
        self.ensure_embedding_model_key()?;
        // Legacy migration: add project_scope to structured tables (new DBs already have it; idempotent)
        self.ensure_structured_scope_columns()?;
        self.ensure_search_text_columns()?;
        self.ensure_consolidation_columns()?;
        self.ensure_pending_turn_columns()?;
        self.backfill_memory_scopes()?;
        // Tantivy FTS index attaches to existing tables (Turso syntax, not an SQLite FTS5 virtual table)
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_working_idx ON working_memory USING fts (content)",
        )?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_episodes_idx ON episodic_memory USING fts (content)",
        )?;
        self.ensure_fts_metadata()?;
        self.recover_fts_state_if_invalid()?;
        // 结构化表 scope 索引：LIKE %q% 无法走索引，但先按 project_scope 裁剪可避免跨项目全表扫描
        for table in ["facts", "preferences", "instructions", "timeline"] {
            let sql =
                format!("CREATE INDEX IF NOT EXISTS idx_{table}_scope ON {table}(project_scope)");
            self.conn.prepare_execute_batch(&sql)?;
        }
        self.prune_completed_outbox_turns()?;
        Ok(())
    }

    /// Reinstall the scope-update triggers with a change guard. The upsert statements always
    /// assign `project_scope`; without the guard, Turso may raise a duplicate registry-key error
    /// when a memory is updated but remains in the same scope.
    fn ensure_scope_registry_update_triggers(&self) -> anyhow::Result<()> {
        self.begin_tx()
            .context("begin FTS backfill completion transaction")?;
        let result = self.conn.prepare_execute_batch(
            "DROP TRIGGER IF EXISTS working_scope_registry_update;
             CREATE TRIGGER working_scope_registry_update
             AFTER UPDATE OF project_scope ON working_memory
             WHEN OLD.project_scope IS NOT NEW.project_scope
             BEGIN
                 INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
                 VALUES ('working_memory', NEW.project_scope);
                 DELETE FROM memory_scopes
                 WHERE memory_table = 'working_memory' AND project_scope = OLD.project_scope
                   AND NOT EXISTS (
                       SELECT 1 FROM working_memory WHERE project_scope = OLD.project_scope
                   );
             END;
             DROP TRIGGER IF EXISTS episodic_scope_registry_update;
             CREATE TRIGGER episodic_scope_registry_update
             AFTER UPDATE OF project_scope ON episodic_memory
             WHEN OLD.project_scope IS NOT NEW.project_scope
             BEGIN
                 INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
                 VALUES ('episodic_memory', NEW.project_scope);
                 DELETE FROM memory_scopes
                 WHERE memory_table = 'episodic_memory' AND project_scope = OLD.project_scope
                   AND NOT EXISTS (
                       SELECT 1 FROM episodic_memory WHERE project_scope = OLD.project_scope
                   );
             END;",
        );
        match result {
            Ok(()) => self.commit_tx(),
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error.into())
            }
        }
    }

    /// Rebuild any incompatible scope registry before installing its table-level schema.
    fn migrate_memory_scope_registry(&self) -> anyhow::Result<()> {
        let mut info = self.conn.prepare("PRAGMA table_info(memory_scopes)")?;
        let columns = info.run_collect_rows()?;
        if columns.is_empty() {
            return Ok(());
        }
        let mut memory_table_pk = None;
        let mut project_scope_pk = None;
        for row in &columns {
            let Some(name) = row.get(1).map(value_to_string) else {
                continue;
            };
            let primary_key_order = row.get(5).map(value_to_i64).unwrap_or_default();
            match name.as_str() {
                "memory_table" => memory_table_pk = Some(primary_key_order),
                "project_scope" => project_scope_pk = Some(primary_key_order),
                _ => {}
            }
        }
        let schema_is_current =
            columns.len() == 2 && memory_table_pk == Some(1) && project_scope_pk == Some(2);
        if schema_is_current {
            return Ok(());
        }

        self.conn.prepare_execute_batch(
            "DROP TRIGGER IF EXISTS working_scope_registry_insert;
             DROP TRIGGER IF EXISTS working_scope_registry_update;
             DROP TRIGGER IF EXISTS working_scope_registry_delete;
             DROP TRIGGER IF EXISTS episodic_scope_registry_insert;
             DROP TRIGGER IF EXISTS episodic_scope_registry_update;
             DROP TRIGGER IF EXISTS episodic_scope_registry_delete;
             DROP TABLE memory_scopes;",
        )?;
        Ok(())
    }

    /// Keep the durable outbox bounded without discarding retryable work: completed turns are
    /// retained for 30 days; exhausted failures remain visible for 90 days.
    fn prune_completed_outbox_turns(&self) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let completed_cutoff =
            (now - chrono::Duration::days(30)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let failed_cutoff =
            (now - chrono::Duration::days(90)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut stmt = self.conn.prepare(
            "DELETE FROM pending_turns
             WHERE (status = 'done' AND updated_at < ?1)
                OR (status = 'failed' AND attempts >= 5 AND updated_at < ?2)",
        )?;
        bind_text(&mut stmt, 1, &completed_cutoff)?;
        bind_text(&mut stmt, 2, &failed_cutoff)?;
        stmt.run_ignore_rows()?;
        let removed = self.conn.changes();
        if removed > 0 {
            crate::debug_log(&format!("pruned {removed} expired outbox rows"));
        }
        Ok(())
    }

    fn ensure_search_text_columns(&self) -> anyhow::Result<()> {
        for table in ["working_memory", "episodic_memory"] {
            let mut info = self.conn.prepare(format!("PRAGMA table_info({table})"))?;
            let has_column = info.run_collect_rows()?.iter().any(|row| {
                matches!(row.get(1), Some(Value::Text(name)) if name.as_str() == "search_text")
            });
            if !has_column {
                if let Err(error) = self.conn.prepare_execute_batch(format!(
                    "ALTER TABLE {table} ADD COLUMN search_text TEXT"
                )) {
                    let message = error.to_string().to_ascii_lowercase();
                    if !message.contains("duplicate column") && !message.contains("already exists")
                    {
                        return Err(error.into());
                    }
                }
            }
        }
        Ok(())
    }

    fn ensure_fts_metadata(&self) -> anyhow::Result<()> {
        let mut info = self.conn.prepare("PRAGMA table_info(fts_migration_lock)")?;
        let columns = info.run_collect_rows()?;
        if !columns.iter().any(
            |row| matches!(row.get(1), Some(Value::Text(name)) if name.as_str() == "fencing_token"),
        ) {
            if let Err(error) = self.conn.prepare_execute_batch(
                "ALTER TABLE fts_migration_lock ADD COLUMN fencing_token INTEGER NOT NULL DEFAULT 0",
            ) {
                let message = error.to_string().to_ascii_lowercase();
                if !message.contains("duplicate column") && !message.contains("already exists") {
                    return Err(error.into());
                }
            }
        }
        self.conn
            .execute("INSERT OR IGNORE INTO fts_migration_lock(id) VALUES (1)")?;
        let active_model_id = crate::embedding_runtime::active_model_id();
        for (key, value) in [
            ("schema_version", "3"),
            ("fts_state", "content"),
            ("fts_version", "1"),
            ("fts_phase", "idle"),
            ("fts_dirty", "false"),
            ("needs_rehydrate", "false"),
            ("index_state", "ok"),
            ("consolidation_dirty", "false"),
            ("embedding_model", active_model_id.as_str()),
        ] {
            if self.get_meta(key)?.is_none() {
                self.set_meta(key, value)?;
            }
        }
        self.set_meta("embedding_model", &active_model_id)?;
        let state = self.get_meta("fts_state")?.unwrap_or_default();
        let phase = self.get_meta("fts_phase")?.unwrap_or_default();
        let expected_phase = if state == "search_text" {
            "search_text"
        } else if phase == "search_text" {
            "idle"
        } else {
            phase.as_str()
        };
        if phase != expected_phase {
            self.set_meta("fts_phase", expected_phase)?;
        }
        Ok(())
    }

    fn recover_fts_state_if_invalid(&self) -> anyhow::Result<()> {
        if self.get_meta("fts_state")?.as_deref() != Some("search_text") {
            return Ok(());
        }
        let null_rows = self.count_null_search_text()?;
        let indexes_exist = self.fts_index_exists("fts_working_idx_v2")?
            && self.fts_index_exists("fts_episodes_idx_v2")?;
        let probe_ok = indexes_exist && self.probe_search_text_indexes().is_ok();
        if null_rows == 0 && probe_ok {
            return Ok(());
        }
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<()> {
            self.set_meta("fts_state", "content")?;
            self.set_meta("fts_version", "1")?;
            self.set_meta("fts_phase", "backfilled")?;
            self.set_meta("fts_dirty", "true")?;
            self.set_meta("index_state", "partial")?;
            self.set_meta(
                "fts_last_error",
                &format!(
                    "search_text invariant failed: null_rows={null_rows}, indexes_exist={indexes_exist}, probe_ok={probe_ok}"
                ),
            )
        })();
        match result {
            Ok(()) => self.commit_tx(),
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    /// Migrate the search-text index when this process owns the lease.
    /// Returns `false` when another process currently owns the lease.
    pub fn migrate_search_text_if_needed(&self) -> anyhow::Result<bool> {
        let owner = format!(
            "{}:{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            std::thread::current().name().unwrap_or("fts")
        );
        let token = match self.acquire_fts_lease(&owner) {
            Ok(Some(token)) => token,
            Ok(None) => return Ok(false),
            Err(error) => {
                crate::debug_log(&format!(
                    "could not acquire FTS migration lease; continuing with current index: {error}"
                ));
                return Err(error);
            }
        };
        let already_ready =
            match self
                .get_meta("fts_state")
                .and_then(|state| match state.as_deref() {
                    Some("search_text") => self
                        .count_null_search_text()
                        .map(|null_rows| null_rows == 0),
                    _ => Ok(false),
                }) {
                Ok(already_ready) => already_ready,
                Err(error) => {
                    let _ = self.release_fts_lease(&owner, token);
                    return Err(error);
                }
            };
        if already_ready {
            self.release_fts_lease(&owner, token)?;
            return Ok(true);
        }

        let phase = self
            .get_meta("fts_phase")?
            .unwrap_or_else(|| "acquired".to_string());
        let migration = self
            .migrate_search_text_under_lease(&owner, token)
            .map_err(|error| anyhow::anyhow!("FTS migration failed during {phase}: {error:#}"));
        if let Err(error) = migration {
            self.record_fts_migration_error(&owner, token, &error.to_string());
            let _ = self.release_fts_lease(&owner, token);
            crate::debug_log(&format!(
                "search_text FTS migration deferred; continuing on content index: {error}"
            ));
            return Err(error);
        }
        Ok(true)
    }

    fn migrate_search_text_under_lease(&self, owner: &str, token: i64) -> anyhow::Result<()> {
        #[cfg(test)]
        let mut backfill_batch_number = 0;
        loop {
            self.begin_tx().context("begin FTS backfill transaction")?;
            let batch_result = (|| -> anyhow::Result<usize> {
                self.renew_fts_lease_in_tx(owner, token)
                    .context("renew FTS lease before backfill batch")?;
                self.set_meta("fts_phase", "backfilling")?;
                let rows = self.conn.prepare(
                    "SELECT 'working', id, content FROM working_memory WHERE search_text IS NULL
                     UNION ALL
                     SELECT 'episodic', id, content FROM episodic_memory WHERE search_text IS NULL
                     LIMIT 100",
                )?.run_collect_rows().context("read FTS backfill batch")?;
                let heartbeat_interval = fts_lease_heartbeat_interval();
                let mut last_heartbeat = std::time::Instant::now();
                for (row_index, row) in rows.iter().enumerate() {
                    if last_heartbeat.elapsed() >= heartbeat_interval {
                        self.renew_fts_lease_in_tx(owner, token).map_err(|error| {
                            anyhow::anyhow!(
                                "FTS migration heartbeat failed at batch row {row_index}: {error}"
                            )
                        })?;
                        last_heartbeat = std::time::Instant::now();
                    }
                    let kind = value_to_string(&row[0]);
                    let id = value_to_string(&row[1]);
                    let content = value_to_string(&row[2]);
                    let query = if kind == "working" {
                        "UPDATE working_memory SET search_text = ?1 WHERE id = ?2 AND search_text IS NULL"
                    } else {
                        "UPDATE episodic_memory SET search_text = ?1 WHERE id = ?2 AND search_text IS NULL"
                    };
                    let mut update = self.conn.prepare(query).context("prepare FTS row update")?;
                    bind_text(&mut update, 1, &crate::text::build_search_text(&content))?;
                    bind_text(&mut update, 2, &id)?;
                    update.run_ignore_rows().context("update FTS row")?;
                    #[cfg(test)]
                    delay_fts_migration_row_for_test();
                }
                Ok(rows.len())
            })();
            let count = match batch_result {
                Ok(count) => {
                    self.commit_tx().context("commit FTS backfill batch")?;
                    count
                }
                Err(error) => {
                    let _ = self.rollback_tx();
                    return Err(error);
                }
            };
            #[cfg(test)]
            {
                backfill_batch_number += 1;
                pause_fts_migration_at_phase_for_test("backfill_batch", backfill_batch_number)?;
            }
            if count < 100 {
                break;
            }
        }

        self.begin_tx()?;
        let phase_result = (|| -> anyhow::Result<()> {
            self.renew_fts_lease_in_tx(owner, token)?;
            anyhow::ensure!(
                self.count_null_search_text()? == 0,
                "search_text backfill incomplete"
            );
            self.set_meta("fts_phase", "backfilled")
        })();
        match phase_result {
            Ok(()) => self.commit_tx()?,
            Err(error) => {
                let _ = self.rollback_tx();
                return Err(error);
            }
        }

        self.assert_fts_lease(owner, token)
            .context("verify FTS lease before index creation")?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_working_idx_v2 ON working_memory USING fts (search_text)",
        ).context("create working search_text FTS index")?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_episodes_idx_v2 ON episodic_memory USING fts (search_text)",
        ).context("create episodic search_text FTS index")?;
        self.probe_search_text_indexes()
            .context("probe search_text FTS indexes")?;
        #[cfg(test)]
        pause_fts_migration_at_phase_for_test("index_ready_before_switch", 0)?;

        self.begin_tx()
            .context("begin FTS state switch transaction")?;
        let switch_result = (|| -> anyhow::Result<()> {
            self.renew_fts_lease_in_tx(owner, token)?;
            anyhow::ensure!(
                self.count_null_search_text()? == 0,
                "search_text rows became incomplete"
            );
            self.set_meta("fts_phase", "index_ready")?;
            self.set_meta("fts_state", "search_text")?;
            self.set_meta("fts_version", "2")?;
            self.set_meta("fts_phase", "search_text")?;
            self.set_meta("fts_dirty", "false")?;
            self.delete_meta("fts_last_error")?;
            self.set_meta("index_state", "ok")?;
            self.release_fts_lease_in_tx(owner, token)
        })();
        match switch_result {
            Ok(()) => self.commit_tx(),
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    fn acquire_fts_lease(&self, owner: &str) -> anyhow::Result<Option<i64>> {
        self.begin_tx()?;
        let now = fts_now();
        let expires = fts_lease_expiry();
        let result = (|| -> anyhow::Result<Option<i64>> {
            let mut update = self.conn.prepare(
                "UPDATE fts_migration_lock SET owner = ?1, acquired_at = ?2, expires_at = ?3,
                    fencing_token = fencing_token + 1
                 WHERE id = 1 AND (owner IS NULL OR expires_at IS NULL OR expires_at <= ?2)",
            )?;
            bind_text(&mut update, 1, owner)?;
            bind_text(&mut update, 2, &now)?;
            bind_text(&mut update, 3, &expires)?;
            update.run_ignore_rows()?;
            if self.conn.changes() != 1 {
                return Ok(None);
            }
            let mut read = self.conn.prepare(
                "SELECT fencing_token FROM fts_migration_lock WHERE id = 1 AND owner = ?1",
            )?;
            bind_text(&mut read, 1, owner)?;
            let rows = read.run_collect_rows()?;
            Ok(rows.first().map(|row| value_to_i64(&row[0])))
        })();
        match result {
            Ok(Some(token)) => {
                self.commit_tx()?;
                Ok(Some(token))
            }
            Ok(None) => {
                self.rollback_tx()?;
                Ok(None)
            }
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    fn renew_fts_lease_in_tx(&self, owner: &str, token: i64) -> anyhow::Result<()> {
        let expires = fts_lease_expiry();
        // Expiry lets a contender attempt takeover; owner+token decide the race. Allowing the
        // unchanged owner to renew after a clock jump is safe because takeover increments token.
        let mut update = self.conn.prepare(
            "UPDATE fts_migration_lock SET expires_at = ?1
             WHERE id = 1 AND owner = ?2 AND fencing_token = ?3",
        )?;
        bind_text(&mut update, 1, &expires)?;
        bind_text(&mut update, 2, owner)?;
        bind_int(&mut update, 3, token)?;
        update.run_ignore_rows()?;
        let changed = self.conn.changes();
        if changed != 1 {
            let mut current = self.conn.prepare(
                "SELECT owner, fencing_token, expires_at FROM fts_migration_lock WHERE id = 1",
            )?;
            let rows = current.run_collect_rows()?;
            let observed = rows.first().map(|row| {
                (
                    value_to_string(&row[0]),
                    value_to_i64(&row[1]),
                    value_to_string(&row[2]),
                )
            });
            anyhow::bail!(
                "FTS migration lease lost (changes={changed}, owner={owner}, token={token}, now={}, observed={observed:?})",
                fts_now()
            );
        }
        Ok(())
    }

    fn assert_fts_lease(&self, owner: &str, token: i64) -> anyhow::Result<()> {
        let mut read = self.conn.prepare(
            "SELECT 1 FROM fts_migration_lock WHERE id = 1 AND owner = ?1
             AND fencing_token = ?2 AND expires_at > ?3",
        )?;
        bind_text(&mut read, 1, owner)?;
        bind_int(&mut read, 2, token)?;
        bind_text(&mut read, 3, &fts_now())?;
        anyhow::ensure!(
            !read.run_collect_rows()?.is_empty(),
            "FTS migration lease lost"
        );
        Ok(())
    }

    fn release_fts_lease_in_tx(&self, owner: &str, token: i64) -> anyhow::Result<()> {
        let mut update = self.conn.prepare(
            "UPDATE fts_migration_lock SET owner = NULL, expires_at = NULL
             WHERE id = 1 AND owner = ?1 AND fencing_token = ?2",
        )?;
        bind_text(&mut update, 1, owner)?;
        bind_int(&mut update, 2, token)?;
        update.run_ignore_rows()?;
        anyhow::ensure!(
            self.conn.changes() == 1,
            "FTS migration lease lost before release"
        );
        Ok(())
    }

    fn release_fts_lease(&self, owner: &str, token: i64) -> anyhow::Result<()> {
        self.begin_tx()?;
        let result = self.release_fts_lease_in_tx(owner, token);
        match result {
            Ok(()) => self.commit_tx(),
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    fn record_fts_migration_error(&self, owner: &str, token: i64, error: &str) {
        if self.begin_tx().is_err() {
            return;
        }
        let result = (|| -> anyhow::Result<()> {
            self.renew_fts_lease_in_tx(owner, token)?;
            self.set_meta(
                "fts_last_error",
                &error.chars().take(500).collect::<String>(),
            )?;
            self.set_meta("fts_dirty", "true")?;
            self.set_meta("index_state", "partial")
        })();
        if result.is_ok() {
            let _ = self.commit_tx();
        } else {
            let _ = self.rollback_tx();
        }
    }

    fn count_null_search_text(&self) -> anyhow::Result<i64> {
        let rows = self
            .conn
            .prepare(
                "SELECT (SELECT COUNT(*) FROM working_memory WHERE search_text IS NULL)
                  + (SELECT COUNT(*) FROM episodic_memory WHERE search_text IS NULL)",
            )?
            .run_collect_rows()?;
        Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
    }

    fn fts_index_exists(&self, name: &str) -> anyhow::Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = ?1 LIMIT 1")?;
        bind_text(&mut stmt, 1, name)?;
        Ok(!stmt.run_collect_rows()?.is_empty())
    }

    fn probe_search_text_indexes(&self) -> anyhow::Result<()> {
        for table in ["working_memory", "episodic_memory"] {
            let sql = format!("SELECT 1 FROM {table} WHERE fts_match(search_text, ?1) LIMIT 1");
            let mut stmt = self.conn.prepare(&sql)?;
            bind_text(&mut stmt, 1, "\"migrationprobe\"")?;
            let _ = stmt.run_collect_rows()?;
        }
        Ok(())
    }

    fn ensure_consolidation_columns(&self) -> anyhow::Result<()> {
        let mut info = self.conn.prepare("PRAGMA table_info(working_memory)")?;
        let rows = info.run_collect_rows()?;
        for column in [
            "consolidation_lease_owner",
            "consolidation_lease_expires_at",
            "consolidation_claimed_at",
        ] {
            let found = rows.iter().any(
                |row| matches!(row.get(1), Some(Value::Text(name)) if name.as_str() == column),
            );
            if !found {
                self.conn.prepare_execute_batch(format!(
                    "ALTER TABLE working_memory ADD COLUMN {column} TEXT"
                ))?;
            }
        }
        Ok(())
    }

    fn ensure_pending_turn_columns(&self) -> anyhow::Result<()> {
        let mut info = self.conn.prepare("PRAGMA table_info(pending_turns)")?;
        let has_column = info.run_collect_rows()?.iter().any(|row| {
            matches!(row.get(1), Some(Value::Text(name)) if name.as_str() == "prompt_missing")
        });
        if !has_column {
            self.conn.prepare_execute_batch(
                "ALTER TABLE pending_turns ADD COLUMN prompt_missing INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        Ok(())
    }

    /// Legacy migration: add the project_scope column to facts/preferences/instructions/timeline (idempotent).
    fn ensure_structured_scope_columns(&self) -> anyhow::Result<()> {
        for table in ["facts", "preferences", "instructions", "timeline"] {
            let sql = format!("PRAGMA table_info({table})");
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.run_collect_rows()?;
            let has_scope = rows
                .iter()
                .any(|row| matches!(row.get(1), Some(Value::Text(name)) if name.as_str() == "project_scope"));
            if !has_scope {
                let alter = format!(
                    "ALTER TABLE {table} ADD COLUMN project_scope TEXT NOT NULL DEFAULT ''"
                );
                self.conn.prepare_execute_batch(&alter)?;
            }
        }
        Ok(())
    }

    /// Preserve one vector per model so switching models and rolling back does not overwrite the
    /// previous model's embedding for each memory.
    fn ensure_embedding_model_key(&self) -> anyhow::Result<()> {
        let rows = self
            .conn
            .prepare("PRAGMA table_info(memory_embeddings)")?
            .run_collect_rows()?;
        let mut primary_key = Vec::<(i64, String)>::new();
        for row in &rows {
            let name = match row.get(1) {
                Some(Value::Text(name)) => name.as_str().to_string(),
                _ => continue,
            };
            let ordinal = row.get(5).map(value_to_i64).unwrap_or(0);
            if ordinal > 0 {
                primary_key.push((ordinal, name));
            }
        }
        primary_key.sort_by_key(|(ordinal, _)| *ordinal);
        if primary_key.iter().map(|(_, name)| name.as_str()).eq([
            "memory_kind",
            "memory_id",
            "model",
        ]) {
            return Ok(());
        }

        self.begin_tx()?;
        let result = (|| -> anyhow::Result<()> {
            self.conn.prepare_execute_batch(
                "DROP TABLE IF EXISTS memory_embeddings_model_key_migration;
                 CREATE TABLE memory_embeddings_model_key_migration (
                    memory_kind TEXT NOT NULL,
                    memory_id TEXT NOT NULL,
                    embedding_json TEXT NOT NULL,
                    model TEXT NOT NULL,
                    created_at TEXT DEFAULT CURRENT_TIMESTAMP,
                    PRIMARY KEY (memory_kind, memory_id, model)
                 );
                 INSERT OR IGNORE INTO memory_embeddings_model_key_migration
                    (memory_kind, memory_id, embedding_json, model, created_at)
                 SELECT memory_kind, memory_id, embedding_json, model, created_at
                 FROM memory_embeddings;
                 DROP TABLE memory_embeddings;
                 ALTER TABLE memory_embeddings_model_key_migration RENAME TO memory_embeddings;",
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => match self.commit_tx() {
                Ok(()) => Ok(()),
                Err(error) => {
                    let _ = self.rollback_tx();
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    // -----------------------------------------------------------------------
    // working_memory
    // -----------------------------------------------------------------------

    /// Insert or update (upsert, matching the TS insertWorkingMemory semantics).
    pub fn insert_working(&self, record: &WorkingMemoryRecord) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO working_memory (
                id, content, embed_text, source, session_id, project_scope,
                importance, memory_type, metadata_json, created_at, updated_at,
                consolidated_at, valid_until, superseded_by, search_text
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                embed_text = excluded.embed_text,
                source = excluded.source,
                session_id = excluded.session_id,
                project_scope = excluded.project_scope,
                importance = excluded.importance,
                memory_type = excluded.memory_type,
                metadata_json = excluded.metadata_json,
                updated_at = excluded.updated_at,
                consolidated_at = excluded.consolidated_at,
                valid_until = excluded.valid_until,
                superseded_by = excluded.superseded_by,
                search_text = excluded.search_text",
        )?;
        bind_text(&mut stmt, 1, &record.id)?;
        bind_text(&mut stmt, 2, &record.content)?;
        bind_opt_text(&mut stmt, 3, record.embed_text.as_deref())?;
        bind_opt_text(&mut stmt, 4, record.source.as_deref())?;
        bind_text(&mut stmt, 5, &record.session_id)?;
        bind_text(&mut stmt, 6, &record.project_scope)?;
        bind_num(&mut stmt, 7, record.importance)?;
        bind_text(&mut stmt, 8, &record.memory_type)?;
        bind_text(&mut stmt, 9, &metadata_to_json(record.metadata.as_ref()))?;
        bind_text(&mut stmt, 10, &record.created_at)?;
        bind_text(&mut stmt, 11, &record.updated_at)?;
        bind_opt_text(&mut stmt, 12, record.consolidated_at.as_deref())?;
        bind_opt_text(&mut stmt, 13, record.valid_until.as_deref())?;
        bind_opt_text(&mut stmt, 14, record.superseded_by.as_deref())?;
        bind_text(
            &mut stmt,
            15,
            &crate::text::build_search_text(&record.content),
        )?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    /// Query a working memory by id.
    pub fn get_working(&self, id: &str) -> anyhow::Result<Option<WorkingMemoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, embed_text, source, session_id, project_scope,
                    importance, memory_type, metadata_json, created_at, updated_at,
                    consolidated_at, valid_until, superseded_by
             FROM working_memory WHERE id = ?1",
        )?;
        bind_text(&mut stmt, 1, id)?;
        collect_first_row(&mut stmt, working_from_row)
    }

    /// Query an episodic memory by id.
    pub fn get_episodic(&self, id: &str) -> anyhow::Result<Option<EpisodicMemoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, summary_of, session_id, project_scope,
                    importance, tier, metadata_json, memory_type,
                    created_at, updated_at, degraded_at, valid_until, superseded_by
             FROM episodic_memory WHERE id = ?1",
        )?;
        bind_text(&mut stmt, 1, id)?;
        collect_first_row(&mut stmt, episodic_from_row)
    }

    /// Check that an evaluation label refers to a memory visible in the requested project.
    pub fn memory_exists_for_project(
        &self,
        kind: MemoryKind,
        id: &str,
        project_scope: &str,
    ) -> anyhow::Result<bool> {
        let (table, id_column, scope_predicate) = match kind {
            MemoryKind::Working => ("working_memory", "id", "project_scope = ?2"),
            MemoryKind::Episodic => ("episodic_memory", "id", "project_scope = ?2"),
            MemoryKind::Fact => (
                "facts",
                "id",
                "(project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            ),
            MemoryKind::Preference => (
                "preferences",
                "id",
                "(project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            ),
            MemoryKind::Instruction => (
                "instructions",
                "id",
                "(project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            ),
            MemoryKind::Timeline => (
                "timeline",
                "event_id",
                "(project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            ),
        };
        let sql =
            format!("SELECT 1 FROM {table} WHERE {id_column} = ?1 AND {scope_predicate} LIMIT 1");
        let mut stmt = self.conn.prepare(&sql)?;
        bind_text(&mut stmt, 1, id)?;
        bind_text(&mut stmt, 2, project_scope)?;
        Ok(!stmt.run_collect_rows()?.is_empty())
    }

    /// Begin a transaction (bulk-write performance: the Tantivy FTS index updates once at COMMIT).
    pub fn begin_tx(&self) -> anyhow::Result<()> {
        self.conn.execute("BEGIN")?;
        Ok(())
    }

    /// Commit the transaction.
    pub fn commit_tx(&self) -> anyhow::Result<()> {
        match self.conn.execute("COMMIT") {
            Ok(()) => Ok(()),
            Err(error) => {
                // A failed commit must not leave this shared connection inside a transaction.
                // The commit may already have taken effect on some backends, so rollback is
                // best-effort and the original commit error remains authoritative.
                let _ = self.conn.execute("ROLLBACK");
                Err(error.into())
            }
        }
    }

    /// Roll back the current transaction (used when a bulk operation fails midway).
    pub fn rollback_tx(&self) -> anyhow::Result<()> {
        self.conn.execute("ROLLBACK")?;
        Ok(())
    }

    pub fn enqueue_pending_turn(
        &self,
        session_id: &str,
        turn_index: i64,
        project_scope: &str,
        text: &str,
        prompt_missing: bool,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            text.len() <= 65_536,
            "pending turn text exceeds 65536 UTF-8 bytes"
        );
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut stmt = self.conn.prepare(
            "INSERT INTO pending_turns(session_id, turn_index, project_scope, text, prompt_missing, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)
             ON CONFLICT(session_id, turn_index) DO NOTHING",
        )?;
        bind_text(&mut stmt, 1, session_id)?;
        bind_int(&mut stmt, 2, turn_index)?;
        bind_text(&mut stmt, 3, project_scope)?;
        bind_text(&mut stmt, 4, text)?;
        bind_int(&mut stmt, 5, prompt_missing as i64)?;
        bind_text(&mut stmt, 6, &now)?;
        stmt.run_ignore_rows()?;
        Ok(self.conn.changes() == 1)
    }

    pub fn claim_pending_turn(
        &self,
        session_id: &str,
        project_scope: &str,
        owner: &str,
        lease_ms: u64,
    ) -> anyhow::Result<Option<PendingTurn>> {
        let now = chrono::Utc::now();
        let now_text = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let expires = (now + chrono::Duration::milliseconds(lease_ms.min(i64::MAX as u64) as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<Option<PendingTurn>> {
            let mut select = self.conn.prepare(
                "SELECT turn_index, project_scope, text, prompt_missing, attempts FROM pending_turns
                 WHERE session_id = ?1 AND project_scope = ?2 AND attempts < 5
                   AND (status IN ('pending', 'failed') OR (status = 'processing' AND lease_expires_at <= ?3))
                   AND (next_attempt_at IS NULL OR next_attempt_at <= ?3)
                 ORDER BY turn_index ASC LIMIT 1",
            )?;
            bind_text(&mut select, 1, session_id)?;
            bind_text(&mut select, 2, project_scope)?;
            bind_text(&mut select, 3, &now_text)?;
            let Some(row) = select.run_collect_rows()?.into_iter().next() else {
                return Ok(None);
            };
            let turn_index = value_to_i64(&row[0]);
            let scope = value_to_string(&row[1]);
            let text = value_to_string(&row[2]);
            let prompt_missing = value_to_i64(&row[3]) != 0;
            let attempts = value_to_i64(&row[4]).max(0) as u32;
            let mut update = self.conn.prepare(
                "UPDATE pending_turns SET status = 'processing', lease_owner = ?1, lease_expires_at = ?2,
                    updated_at = ?3
                 WHERE session_id = ?4 AND turn_index = ?5 AND project_scope = ?6 AND attempts < 5
                   AND (status IN ('pending', 'failed') OR (status = 'processing' AND lease_expires_at <= ?3))
                   AND (next_attempt_at IS NULL OR next_attempt_at <= ?3)",
            )?;
            bind_text(&mut update, 1, owner)?;
            bind_text(&mut update, 2, &expires)?;
            bind_text(&mut update, 3, &now_text)?;
            bind_text(&mut update, 4, session_id)?;
            bind_int(&mut update, 5, turn_index)?;
            bind_text(&mut update, 6, project_scope)?;
            update.run_ignore_rows()?;
            if self.conn.changes() != 1 {
                return Ok(None);
            }
            Ok(Some(PendingTurn {
                session_id: session_id.to_string(),
                turn_index,
                project_scope: scope,
                text,
                prompt_missing,
                attempts,
                lease_owner: owner.to_string(),
                lease_expires_at: expires,
            }))
        })();
        match result {
            Ok(turn) => match self.commit_tx() {
                Ok(()) => Ok(turn),
                Err(error) => {
                    let _ = self.rollback_tx();
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    pub fn heartbeat_pending_turn(
        &self,
        session_id: &str,
        turn_index: i64,
        owner: &str,
        lease_ms: u64,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now();
        let now_text = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let expires = (now + chrono::Duration::milliseconds(lease_ms.min(i64::MAX as u64) as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut stmt = self.conn.prepare(
            "UPDATE pending_turns SET lease_expires_at = ?1, updated_at = ?2
             WHERE session_id = ?3 AND turn_index = ?4 AND status = 'processing'
               AND lease_owner = ?5 AND lease_expires_at > ?2",
        )?;
        bind_text(&mut stmt, 1, &expires)?;
        bind_text(&mut stmt, 2, &now_text)?;
        bind_text(&mut stmt, 3, session_id)?;
        bind_int(&mut stmt, 4, turn_index)?;
        bind_text(&mut stmt, 5, owner)?;
        stmt.run_ignore_rows()?;
        Ok(self.conn.changes() == 1)
    }

    pub fn complete_pending_turn(
        &self,
        session_id: &str,
        turn_index: i64,
        owner: &str,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut stmt = self.conn.prepare(
            "UPDATE pending_turns SET status = 'done', lease_owner = NULL, lease_expires_at = NULL,
                last_error = NULL, next_attempt_at = NULL, updated_at = ?1
             WHERE session_id = ?2 AND turn_index = ?3 AND status = 'processing' AND lease_owner = ?4",
        )?;
        bind_text(&mut stmt, 1, &now)?;
        bind_text(&mut stmt, 2, session_id)?;
        bind_int(&mut stmt, 3, turn_index)?;
        bind_text(&mut stmt, 4, owner)?;
        stmt.run_ignore_rows()?;
        Ok(self.conn.changes() == 1)
    }

    pub fn fail_pending_turn(
        &self,
        session_id: &str,
        turn_index: i64,
        owner: &str,
        error: &str,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now();
        let now_text = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut stmt = self.conn.prepare(
            "UPDATE pending_turns SET status = 'failed', attempts = attempts + 1,
                lease_owner = NULL, lease_expires_at = NULL, last_error = ?1,
                next_attempt_at = ?2, updated_at = ?3
             WHERE session_id = ?4 AND turn_index = ?5 AND status = 'processing' AND lease_owner = ?6",
        )?;
        bind_text(&mut stmt, 1, &error.chars().take(2_000).collect::<String>())?;
        // Backoff is computed from the current failure count in SQL-independent code by reading the row.
        let mut read = self.conn.prepare("SELECT attempts FROM pending_turns WHERE session_id = ?1 AND turn_index = ?2 AND lease_owner = ?3")?;
        bind_text(&mut read, 1, session_id)?;
        bind_int(&mut read, 2, turn_index)?;
        bind_text(&mut read, 3, owner)?;
        let attempts = read
            .run_collect_rows()?
            .first()
            .map(|row| value_to_i64(&row[0]))
            .unwrap_or(0)
            .max(0) as u32;
        let delay = 1u64.checked_shl(attempts.min(8)).unwrap_or(256).min(300);
        let next_attempt = (now + chrono::Duration::seconds(delay as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        bind_text(&mut stmt, 2, &next_attempt)?;
        bind_text(&mut stmt, 3, &now_text)?;
        bind_text(&mut stmt, 4, session_id)?;
        bind_int(&mut stmt, 5, turn_index)?;
        bind_text(&mut stmt, 6, owner)?;
        stmt.run_ignore_rows()?;
        Ok(self.conn.changes() == 1)
    }

    pub fn list_pending_turns(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT turn_index, project_scope, status, attempts, next_attempt_at, last_error
             FROM pending_turns WHERE session_id = ?1 AND status != 'done'
             ORDER BY turn_index LIMIT ?2",
        )?;
        bind_text(&mut stmt, 1, session_id)?;
        bind_int(&mut stmt, 2, limit.clamp(1, 500) as i64)?;
        stmt.run_collect_rows()?
            .iter()
            .map(|row| {
                Ok(serde_json::json!({
                    "turnIndex": value_to_i64(&row[0]),
                    "projectScope": value_to_string(&row[1]),
                    "status": value_to_string(&row[2]),
                    "attempts": value_to_i64(&row[3]),
                    "nextAttemptAt": value_to_opt_string(&row[4]),
                    "lastError": value_to_opt_string(&row[5]),
                }))
            })
            .collect()
    }

    fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO app_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )?;
        bind_text(&mut stmt, 1, key)?;
        bind_text(&mut stmt, 2, value)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    pub fn mark_indexes_ready(&self) -> anyhow::Result<()> {
        self.set_meta("fts_dirty", "false")?;
        self.set_meta("needs_rehydrate", "false")?;
        self.delete_meta("rehydrate_last_error")?;
        self.set_meta("index_state", "ok")
    }

    pub fn mark_vector_rehydrate_ready(&self) -> anyhow::Result<()> {
        self.set_meta("needs_rehydrate", "false")?;
        self.delete_meta("rehydrate_last_error")?;
        let fts_dirty = self.get_meta("fts_dirty")?.as_deref() == Some("true");
        self.set_meta("index_state", if fts_dirty { "partial" } else { "ok" })
    }

    pub fn mark_vector_rehydrate_failed(&self, error: &str) -> anyhow::Result<()> {
        self.set_meta("needs_rehydrate", "true")?;
        self.set_meta("index_state", "partial")?;
        self.set_meta("rehydrate_last_error", error)
    }

    pub fn vector_rehydrate_needed(&self) -> anyhow::Result<bool> {
        Ok(self.get_meta("needs_rehydrate")?.as_deref() == Some("true"))
    }

    pub fn fts_rebuild_needed(&self) -> anyhow::Result<bool> {
        Ok(self.get_meta("fts_dirty")?.as_deref() == Some("true"))
    }

    fn get_meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM app_meta WHERE key = ?1")?;
        bind_text(&mut stmt, 1, key)?;
        let rows = stmt.run_collect_rows()?;
        Ok(rows.first().map(|row| value_to_string(&row[0])))
    }

    pub fn mark_consolidation_dirty(&self) -> anyhow::Result<()> {
        self.set_meta("consolidation_dirty", "true")
    }

    pub fn is_consolidation_dirty(&self) -> anyhow::Result<bool> {
        Ok(self.get_meta("consolidation_dirty")?.as_deref() == Some("true"))
    }

    pub fn clear_consolidation_dirty_if_idle(&self, project_scope: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "UPDATE app_meta SET value = 'false'
             WHERE key = 'consolidation_dirty'
               AND NOT EXISTS (
                   SELECT 1 FROM working_memory
                   WHERE project_scope = ?1 AND consolidated_at IS NULL
               )",
        )?;
        bind_text(&mut stmt, 1, project_scope)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    fn active_fts_column(&self) -> anyhow::Result<&'static str> {
        let configured_mode = std::env::var("PI_MEMORY_FTS_MODE").ok();
        let persisted_state = self.get_meta("fts_state")?;
        active_fts_column_for_mode(configured_mode.as_deref(), persisted_state.as_deref())
    }

    fn delete_meta(&self, key: &str) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare("DELETE FROM app_meta WHERE key = ?1")?;
        bind_text(&mut stmt, 1, key)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    /// Migrate data from a legacy pi-memory SQLite database (M3 migration tool).
    /// The legacy DB is read directly by turso_core (SQLite file compatible); FTS5 virtual tables are ignored (the new Tantivy index rebuilds automatically).
    /// Import runs in one destination transaction, then rebuilds FTS once while preserving
    /// the active reader and recording a durable partial state for vector rehydration.
    pub fn migrate_from(&self, source_path: &str) -> anyhow::Result<serde_json::Value> {
        self.migrate_from_scope(source_path, None)
    }

    pub fn migrate_from_scope(
        &self,
        source_path: &str,
        target_scope: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        let source = TursoStorage::open_readonly(source_path)?;
        let skipped_scope_report = target_scope
            .map(|scope| source.migration_skipped_scope_report(scope))
            .transpose()?;
        let mut stats = serde_json::Map::new();
        if let Some(report) = skipped_scope_report {
            stats.insert("skipped_scopes".into(), report["scopes"].clone());
            stats.insert(
                "skipped_rows_by_table".into(),
                report["rowsByTable"].clone(),
            );
            stats.insert("skipped_rows".into(), report["totalRows"].clone());
        }

        // A new/empty destination has no FTS results to preserve. Pause its FTS indexes in
        // the import transaction so a large import doesn't maintain them once per row; build
        // the content fallback once before commit, then build search_text after commit. DDL
        // and row writes are transactional, so an import failure restores the original indexes.
        // Existing destinations keep their live indexes during the import.
        let mut pause_empty_destination_indexes = false;
        self.begin_tx()?;

        let result = (|| -> anyhow::Result<()> {
            pause_empty_destination_indexes =
                self.count_working()? == 0 && self.count_episodic()? == 0;
            if pause_empty_destination_indexes {
                self.drop_fts()?;
            }
            self.set_meta("fts_dirty", "true")?;
            self.set_meta("needs_rehydrate", "true")?;
            self.set_meta("index_state", "partial")?;
            // working_memory
            let rows = source
                .list_working_for_migration(target_scope)?
                .into_iter()
                .collect::<Vec<_>>();
            let working_ids = rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<std::collections::HashSet<_>>();
            let working_count =
                self.expected_migration_count("working_memory", "id", &working_ids)?;
            for row in rows {
                self.insert_working(&row)?;
            }
            self.verify_migration_count("working_memory", working_count)?;
            stats.insert("working".into(), serde_json::json!(working_ids.len()));

            // episodic_memory
            let rows = source
                .list_episodic_for_migration(target_scope)?
                .into_iter()
                .collect::<Vec<_>>();
            let episodic_ids = rows
                .iter()
                .map(|row| row.id.clone())
                .collect::<std::collections::HashSet<_>>();
            let episodic_count =
                self.expected_migration_count("episodic_memory", "id", &episodic_ids)?;
            for row in rows {
                self.insert_episodic(&row)?;
            }
            self.verify_migration_count("episodic_memory", episodic_count)?;
            stats.insert("episodic".into(), serde_json::json!(episodic_ids.len()));

            // facts / preferences / instructions / timeline
            let facts = source.list_facts_for_migration(target_scope)?;
            let fact_ids = facts.iter().map(|row| row.fact_id.clone()).collect();
            let facts_count = self.expected_migration_count("facts", "fact_id", &fact_ids)?;
            self.insert_facts(&facts)?;
            self.verify_migration_count("facts", facts_count)?;
            stats.insert("facts".into(), serde_json::json!(facts.len()));
            let preferences = source.list_preferences_for_migration(target_scope)?;
            let preference_ids = preferences.iter().map(|row| row.id.clone()).collect();
            let preferences_count =
                self.expected_migration_count("preferences", "id", &preference_ids)?;
            self.insert_preferences(&preferences)?;
            self.verify_migration_count("preferences", preferences_count)?;
            stats.insert("preferences".into(), serde_json::json!(preferences.len()));
            let instructions = source.list_instructions_for_migration(target_scope)?;
            let instruction_ids = instructions.iter().map(|row| row.id.clone()).collect();
            let instructions_count =
                self.expected_migration_count("instructions", "id", &instruction_ids)?;
            self.insert_instructions(&instructions)?;
            self.verify_migration_count("instructions", instructions_count)?;
            stats.insert("instructions".into(), serde_json::json!(instructions.len()));
            let timeline = source.list_timeline_for_migration(target_scope)?;
            let timeline_ids = timeline.iter().map(|row| row.event_id.clone()).collect();
            let timeline_count =
                self.expected_migration_count("timeline", "event_id", &timeline_ids)?;
            self.insert_timeline_events(&timeline)?;
            self.verify_migration_count("timeline", timeline_count)?;
            stats.insert("timeline".into(), serde_json::json!(timeline.len()));

            // embeddings (vectors migrate directly; model metadata preserved)
            let source_embeddings = source.list_embeddings_for_migration(target_scope)?;
            let invalid_embedding_count = source_embeddings
                .iter()
                .filter(|embedding| {
                    embedding.embedding.is_empty()
                        || !embedding
                            .embedding
                            .iter()
                            .all(|value| value.is_finite() && (*value as f32).is_finite())
                })
                .count();
            let embeddings = source_embeddings
                .into_iter()
                .filter(|embedding| {
                    !embedding.embedding.is_empty()
                        && embedding
                            .embedding
                            .iter()
                            .all(|value| value.is_finite() && (*value as f32).is_finite())
                })
                .filter(|embedding| match embedding.memory_kind {
                    MemoryKind::Working => working_ids.contains(&embedding.memory_id),
                    MemoryKind::Episodic => episodic_ids.contains(&embedding.memory_id),
                    _ => false,
                })
                .collect::<Vec<_>>();
            let embedding_count = embeddings.len();
            for embedding in &embeddings {
                self.save_embedding(embedding)?;
            }
            stats.insert("embeddings".into(), serde_json::json!(embedding_count));
            stats.insert(
                "embeddings_skipped_invalid".into(),
                serde_json::json!(invalid_embedding_count),
            );
            if pause_empty_destination_indexes {
                self.conn.prepare_execute_batch(
                    "CREATE INDEX IF NOT EXISTS fts_working_idx ON working_memory USING fts (content);
                     CREATE INDEX IF NOT EXISTS fts_episodes_idx ON episodic_memory USING fts (content);",
                )?;
            }
            Ok(())
        })();

        if let Err(e) = result {
            // FTS writes are part of the same database transaction and roll back with the rows.
            let _ = self.rollback_tx();
            return Err(e);
        }
        self.commit_tx()?;
        // Rebuild the FTS index (one full build after migration). The DB import is already
        // committed, so report partial state instead of claiming rollback if indexing fails.
        match self.rebuild_fts() {
            Ok(()) => {
                self.set_meta("fts_dirty", "false")?;
                stats.insert("index_state".into(), serde_json::json!("partial"));
                stats.insert("needs_rehydrate".into(), serde_json::json!(true));
                stats.insert("db_committed".into(), serde_json::json!(true));
                Ok(serde_json::Value::Object(stats))
            }
            Err(error) => {
                stats.insert("index_state".into(), serde_json::json!("partial"));
                stats.insert("fts_dirty".into(), serde_json::json!(true));
                stats.insert("needs_rehydrate".into(), serde_json::json!(true));
                stats.insert("db_committed".into(), serde_json::json!(true));
                stats.insert("error".into(), serde_json::json!(error.to_string()));
                Ok(serde_json::Value::Object(stats))
            }
        }
    }

    fn migration_skipped_scope_report(
        &self,
        target_scope: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let mut skipped_scopes = std::collections::BTreeSet::new();
        let mut skipped_rows_by_table = serde_json::Map::new();
        let mut total_skipped = 0_u64;
        for table in [
            "working_memory",
            "episodic_memory",
            "facts",
            "preferences",
            "instructions",
            "timeline",
        ] {
            let sql = format!("SELECT project_scope, COUNT(*) FROM {table} GROUP BY project_scope");
            let mut stmt = self.conn.prepare(&sql)?;
            let mut table_skipped = 0_u64;
            for row in stmt.run_collect_rows()? {
                let scope = value_to_string(&row[0]);
                if scope == target_scope {
                    continue;
                }
                skipped_scopes.insert(scope);
                table_skipped += value_to_i64(&row[1]).max(0) as u64;
            }
            total_skipped += table_skipped;
            skipped_rows_by_table.insert(table.to_string(), serde_json::json!(table_skipped));
        }
        Ok(serde_json::json!({
            "scopes": skipped_scopes,
            "rowsByTable": skipped_rows_by_table,
            "totalRows": total_skipped,
        }))
    }

    fn expected_migration_count(
        &self,
        table: &str,
        id_column: &str,
        source_ids: &std::collections::HashSet<String>,
    ) -> anyhow::Result<i64> {
        let before = count_rows(&self.conn, table)?;
        let mut existing = 0_i64;
        let ids = source_ids.iter().collect::<Vec<_>>();
        for chunk in ids.chunks(400) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = (1..=chunk.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE {id_column} IN ({placeholders})");
            let mut stmt = self.conn.prepare(&sql)?;
            for (index, id) in chunk.iter().enumerate() {
                bind_text(&mut stmt, index + 1, id)?;
            }
            let rows = stmt.run_collect_rows()?;
            existing += rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0);
        }
        Ok(before + source_ids.len() as i64 - existing)
    }

    fn verify_migration_count(&self, table: &str, expected: i64) -> anyhow::Result<()> {
        let actual = count_rows(&self.conn, table)?;
        anyhow::ensure!(
            actual == expected,
            "migration count mismatch for {table}: expected {expected}, got {actual}"
        );
        Ok(())
    }

    /// List all working memories (for migration).
    pub fn list_all_working(&self) -> anyhow::Result<Vec<WorkingMemoryRecord>> {
        self.list_working_for_migration(None)
    }

    fn list_working_for_migration(
        &self,
        scope: Option<&str>,
    ) -> anyhow::Result<Vec<WorkingMemoryRecord>> {
        let sql = format!(
            "SELECT id, content, embed_text, source, session_id, project_scope,\n                    importance, memory_type, metadata_json, created_at, updated_at,\n                    consolidated_at, valid_until, superseded_by\n             FROM working_memory{}",
            if scope.is_some() {
                " WHERE project_scope = ?1"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, working_from_row)
    }

    pub fn list_all_episodic(&self) -> anyhow::Result<Vec<EpisodicMemoryRecord>> {
        self.list_episodic_for_migration(None)
    }

    fn list_episodic_for_migration(
        &self,
        scope: Option<&str>,
    ) -> anyhow::Result<Vec<EpisodicMemoryRecord>> {
        let sql = format!(
            "SELECT id, content, summary_of, session_id, project_scope,\n                    importance, tier, metadata_json, memory_type,\n                    created_at, updated_at, degraded_at, valid_until, superseded_by\n             FROM episodic_memory{}",
            if scope.is_some() {
                " WHERE project_scope = ?1"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, episodic_from_row)
    }

    pub fn list_all_facts(&self) -> anyhow::Result<Vec<FactRecord>> {
        self.list_facts_for_migration(None)
    }

    fn list_facts_for_migration(&self, scope: Option<&str>) -> anyhow::Result<Vec<FactRecord>> {
        let sql = format!(
            "SELECT fact_id, subject, predicate, object, confidence, source_memory_id, session_id, project_scope, timestamp FROM facts{}",
            if scope.is_some() {
                " WHERE project_scope = ?1"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, |row| {
            Ok(FactRecord {
                fact_id: row_to_string(row, 0)?,
                subject: row_to_string(row, 1)?,
                predicate: row_to_string(row, 2)?,
                object: row_to_string(row, 3)?,
                confidence: row_to_f64(row, 4)?,
                source_memory_id: row_to_opt_string(row, 5)?,
                session_id: row_to_string(row, 6)?,
                project_scope: row_to_string(row, 7)?,
                timestamp: row_to_string(row, 8)?,
            })
        })
    }

    pub fn list_all_preferences(&self) -> anyhow::Result<Vec<PreferenceRecord>> {
        self.list_preferences_for_migration(None)
    }

    fn list_preferences_for_migration(
        &self,
        scope: Option<&str>,
    ) -> anyhow::Result<Vec<PreferenceRecord>> {
        let sql = format!(
            "SELECT id, preference, topic, context_snippet, source_memory_id, session_id, project_scope, created_at FROM preferences{}",
            if scope.is_some() {
                " WHERE project_scope = ?1"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, |row| {
            Ok(PreferenceRecord {
                id: row_to_string(row, 0)?,
                preference: row_to_string(row, 1)?,
                topic: row_to_opt_string(row, 2)?,
                context_snippet: row_to_opt_string(row, 3)?,
                source_memory_id: row_to_opt_string(row, 4)?,
                session_id: row_to_string(row, 5)?,
                project_scope: row_to_string(row, 6)?,
                created_at: row_to_string(row, 7)?,
            })
        })
    }

    pub fn list_all_instructions(&self) -> anyhow::Result<Vec<InstructionRecord>> {
        self.list_instructions_for_migration(None)
    }

    fn list_instructions_for_migration(
        &self,
        scope: Option<&str>,
    ) -> anyhow::Result<Vec<InstructionRecord>> {
        let sql = format!(
            "SELECT id, instruction, topic, active, context_snippet, source_memory_id, session_id, project_scope, created_at FROM instructions{}",
            if scope.is_some() {
                " WHERE project_scope = ?1"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, |row| {
            Ok(InstructionRecord {
                id: row_to_string(row, 0)?,
                instruction: row_to_string(row, 1)?,
                topic: row_to_opt_string(row, 2)?,
                active: row_to_i64(row, 3) != 0,
                context_snippet: row_to_opt_string(row, 4)?,
                source_memory_id: row_to_opt_string(row, 5)?,
                session_id: row_to_string(row, 6)?,
                project_scope: row_to_string(row, 7)?,
                created_at: row_to_string(row, 8)?,
            })
        })
    }

    pub fn list_all_timeline(&self) -> anyhow::Result<Vec<TimelineEventRecord>> {
        self.list_timeline_for_migration(None)
    }

    fn list_timeline_for_migration(
        &self,
        scope: Option<&str>,
    ) -> anyhow::Result<Vec<TimelineEventRecord>> {
        let sql = format!(
            "SELECT event_id, date, description, source, source_memory_id, session_id, project_scope, created_at FROM timeline{}",
            if scope.is_some() {
                " WHERE project_scope = ?1"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, |row| {
            Ok(TimelineEventRecord {
                event_id: row_to_string(row, 0)?,
                date: row_to_string(row, 1)?,
                description: row_to_string(row, 2)?,
                source: row_to_opt_string(row, 3)?,
                source_memory_id: row_to_opt_string(row, 4)?,
                session_id: row_to_string(row, 5)?,
                project_scope: row_to_string(row, 6)?,
                created_at: row_to_string(row, 7)?,
            })
        })
    }

    pub fn list_all_embeddings(&self) -> anyhow::Result<Vec<MemoryEmbeddingRecord>> {
        self.list_embeddings_for_migration(None)
    }

    /// Read a bounded page of current-model embeddings that still have a source memory row.
    /// Keyset pagination keeps vector hydration's temporary memory independent of project size.
    pub fn list_embedding_hydration_page(
        &self,
        model: &str,
        after_kind: &str,
        after_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<MemoryEmbeddingRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT embedding.memory_kind, embedding.memory_id, embedding.embedding_json, embedding.model
             FROM memory_embeddings AS embedding
             WHERE embedding.model = ?1
               AND ((embedding.memory_kind = 'working' AND EXISTS (
                        SELECT 1 FROM working_memory AS memory WHERE memory.id = embedding.memory_id
                    )) OR (embedding.memory_kind = 'episodic' AND EXISTS (
                        SELECT 1 FROM episodic_memory AS memory WHERE memory.id = embedding.memory_id
                    )))
               AND (embedding.memory_kind > ?2 OR
                    (embedding.memory_kind = ?2 AND embedding.memory_id > ?3))
             ORDER BY embedding.memory_kind, embedding.memory_id
             LIMIT ?4",
        )?;
        bind_text(&mut stmt, 1, model)?;
        bind_text(&mut stmt, 2, after_kind)?;
        bind_text(&mut stmt, 3, after_id)?;
        bind_int(&mut stmt, 4, limit.clamp(1, 4096) as i64)?;
        collect_rows(&mut stmt, |row| {
            let kind = row_to_string(row, 0)?;
            let memory_kind = match kind.as_str() {
                "working" => MemoryKind::Working,
                "episodic" => MemoryKind::Episodic,
                _ => anyhow::bail!("unsupported vector hydration kind: {kind}"),
            };
            Ok(MemoryEmbeddingRecord {
                memory_kind,
                memory_id: row_to_string(row, 1)?,
                embedding: serde_json::from_str(&row_to_string(row, 2)?)?,
                model: row_to_string(row, 3)?,
            })
        })
    }

    fn list_embeddings_for_migration(
        &self,
        scope: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEmbeddingRecord>> {
        let sql = format!(
            "SELECT memory_kind, memory_id, embedding_json, model FROM memory_embeddings{}",
            if scope.is_some() {
                " WHERE (memory_kind = 'working' AND memory_id IN (SELECT id FROM working_memory WHERE project_scope = ?1)) OR (memory_kind = 'episodic' AND memory_id IN (SELECT id FROM episodic_memory WHERE project_scope = ?1))"
            } else {
                ""
            }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        if let Some(scope) = scope {
            bind_text(&mut stmt, 1, scope)?;
        }
        collect_rows(&mut stmt, |row| {
            let kind = row_to_string(row, 0)?;
            let memory_kind = match kind.as_str() {
                "working" => MemoryKind::Working,
                "episodic" => MemoryKind::Episodic,
                _ => MemoryKind::Fact,
            };
            Ok(MemoryEmbeddingRecord {
                memory_kind,
                memory_id: row_to_string(row, 1)?,
                embedding: serde_json::from_str(&row_to_string(row, 2)?)?,
                model: row_to_string(row, 3)?,
            })
        })
    }

    /// List working memories for a project (created_at DESC).
    pub fn list_working_by_project(
        &self,
        project_scope: &str,
    ) -> anyhow::Result<Vec<WorkingMemoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, embed_text, source, session_id, project_scope,
                    importance, memory_type, metadata_json, created_at, updated_at,
                    consolidated_at, valid_until, superseded_by
             FROM working_memory WHERE project_scope = ?1 ORDER BY created_at DESC",
        )?;
        bind_text(&mut stmt, 1, project_scope)?;
        collect_rows(&mut stmt, working_from_row)
    }

    /// Run a cached prepared statement: prepare once, then reset + clear bindings before
    /// each execution (avoids leftover bindings). SQL must be a static literal to hit the cache.
    fn with_cached_stmt<T>(
        &self,
        sql: &str,
        run: impl FnOnce(&mut Statement) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut cache = match self.stmts.lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                self.stmts.clear_poison();
                crate::debug_log("prepared statement cache mutex poisoned; recovering cache");
                poisoned.into_inner()
            }
        };
        if !cache.contains_key(sql) {
            cache.insert(sql.to_string(), self.conn.prepare(sql)?);
        }
        let stmt = cache.get_mut(sql).expect("just inserted");
        stmt.reset()?;
        stmt.clear_bindings();
        run(stmt)
    }

    /// Tantivy FTS search over working memories (matches the TS searchWorkingMemory: MATCH + scope filter + score sort).
    pub fn search_working_fts(
        &self,
        query: &str,
        project_scope: &str,
    ) -> anyhow::Result<Vec<(WorkingMemoryRecord, f64)>> {
        let column = self.active_fts_column()?;
        let fts_query = fts_query_for_column(column, query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, content, embed_text, source, session_id, project_scope,
                    importance, memory_type, metadata_json, created_at, updated_at,
                    consolidated_at, valid_until, superseded_by,
                    fts_score({column}, ?1) AS score
             FROM working_memory
             WHERE fts_match({column}, ?1) AND project_scope = ?2
             ORDER BY score DESC"
        );
        self.with_cached_stmt(&sql, |stmt| {
            bind_text(stmt, 1, &fts_query)?;
            bind_text(stmt, 2, project_scope)?;
            collect_rows_with_score(stmt, working_from_row)
        })
    }

    pub fn search_working_fts_top_k(
        &self,
        query: &str,
        project_scope: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(WorkingMemoryRecord, f64)>> {
        if self.has_other_scope("working_memory", project_scope)? {
            let mut hits = self.search_working_fts(query, project_scope)?;
            hits.truncate(limit);
            return Ok(hits);
        }
        let column = self.active_fts_column()?;
        let fts_query = fts_query_for_column(column, query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, content, embed_text, source, session_id, project_scope,
                    importance, memory_type, metadata_json, created_at, updated_at,
                    consolidated_at, valid_until, superseded_by,
                    fts_score({column}, ?1) AS score
             FROM working_memory
             WHERE fts_match({column}, ?1)
             ORDER BY score DESC
             LIMIT ?2"
        );
        self.with_cached_stmt(&sql, |stmt| {
            bind_text(stmt, 1, &fts_query)?;
            bind_int(stmt, 2, limit.min(i64::MAX as usize) as i64)?;
            collect_rows_with_score(stmt, working_from_row)
        })
    }

    /// Return Turso's query-plan description for the exact multi-column Top-K working-memory
    /// query used by recall. Selecting only `id` can hide sorter/full-scan regressions from the
    /// production projection, so keep this SQL aligned with `search_working_fts_top_k`.
    pub fn explain_working_fts_top_k(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let column = self.active_fts_column()?;
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT id, content, embed_text, source, session_id, project_scope,
                    importance, memory_type, metadata_json, created_at, updated_at,
                    consolidated_at, valid_until, superseded_by,
                    fts_score({column}, ?1) AS score
             FROM working_memory
             WHERE fts_match({column}, ?1)
             ORDER BY score DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        bind_text(&mut stmt, 1, query)?;
        bind_int(&mut stmt, 2, limit.min(i64::MAX as usize) as i64)?;
        Ok(stmt
            .run_collect_rows()?
            .iter()
            .map(|row| row.get(3).map(value_to_string).unwrap_or_default())
            .collect())
    }

    fn has_other_scope(&self, table: &str, project_scope: &str) -> anyhow::Result<bool> {
        let table = match table {
            "working_memory" => "working_memory",
            "episodic_memory" => "episodic_memory",
            _ => anyhow::bail!("unsupported recall scope table: {table}"),
        };
        let mut stmt = self.conn.prepare(
            "SELECT 1 FROM memory_scopes WHERE memory_table = ?1 AND project_scope != ?2 LIMIT 1",
        )?;
        bind_text(&mut stmt, 1, table)?;
        bind_text(&mut stmt, 2, project_scope)?;
        Ok(!stmt.run_collect_rows()?.is_empty())
    }

    /// Whether this legacy database contains recallable memories from another named scope.
    /// Empty-scope legacy rows remain visible for compatibility and do not trigger the slow path.
    pub fn has_other_recall_scope(&self, project_scope: &str) -> anyhow::Result<bool> {
        let mut stmt = self.conn.prepare(
            "SELECT 1 FROM memory_scopes
             WHERE project_scope != '' AND project_scope != ?1 LIMIT 1",
        )?;
        bind_text(&mut stmt, 1, project_scope)?;
        Ok(!stmt.run_collect_rows()?.is_empty())
    }

    fn backfill_memory_scopes(&self) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO memory_scopes(memory_table, project_scope)
             SELECT 'working_memory', project_scope FROM working_memory
             UNION SELECT 'episodic_memory', project_scope FROM episodic_memory",
        )?;
        Ok(())
    }

    /// Count working_memory rows.
    pub fn count_working(&self) -> anyhow::Result<i64> {
        count_rows(&self.conn, "working_memory")
    }

    // -----------------------------------------------------------------------
    // episodic_memory
    // -----------------------------------------------------------------------

    /// Insert or update an episodic memory.
    pub fn insert_episodic(&self, record: &EpisodicMemoryRecord) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO episodic_memory (
                id, content, summary_of, session_id, project_scope,
                importance, tier, metadata_json, memory_type,
                created_at, updated_at, degraded_at, valid_until, superseded_by, search_text
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
            ON CONFLICT(id) DO UPDATE SET
                content = excluded.content,
                summary_of = excluded.summary_of,
                session_id = excluded.session_id,
                project_scope = excluded.project_scope,
                importance = excluded.importance,
                tier = excluded.tier,
                metadata_json = excluded.metadata_json,
                memory_type = excluded.memory_type,
                updated_at = excluded.updated_at,
                degraded_at = excluded.degraded_at,
                valid_until = excluded.valid_until,
                superseded_by = excluded.superseded_by,
                search_text = excluded.search_text",
        )?;
        bind_text(&mut stmt, 1, &record.id)?;
        bind_text(&mut stmt, 2, &record.content)?;
        bind_text(&mut stmt, 3, &record.summary_of)?;
        bind_text(&mut stmt, 4, &record.session_id)?;
        bind_text(&mut stmt, 5, &record.project_scope)?;
        bind_num(&mut stmt, 6, record.importance)?;
        bind_int(&mut stmt, 7, record.tier)?;
        bind_text(&mut stmt, 8, &metadata_to_json(record.metadata.as_ref()))?;
        bind_text(&mut stmt, 9, &record.memory_type)?;
        bind_text(&mut stmt, 10, &record.created_at)?;
        bind_text(&mut stmt, 11, &record.updated_at)?;
        bind_opt_text(&mut stmt, 12, record.degraded_at.as_deref())?;
        bind_opt_text(&mut stmt, 13, record.valid_until.as_deref())?;
        bind_opt_text(&mut stmt, 14, record.superseded_by.as_deref())?;
        bind_text(
            &mut stmt,
            15,
            &crate::text::build_search_text(&record.content),
        )?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    /// List episodic memories for a project (created_at DESC).
    pub fn list_episodic_by_project(
        &self,
        project_scope: &str,
    ) -> anyhow::Result<Vec<EpisodicMemoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, summary_of, session_id, project_scope,
                    importance, tier, metadata_json, memory_type,
                    created_at, updated_at, degraded_at, valid_until, superseded_by
             FROM episodic_memory WHERE project_scope = ?1 ORDER BY created_at DESC",
        )?;
        bind_text(&mut stmt, 1, project_scope)?;
        collect_rows(&mut stmt, episodic_from_row)
    }

    /// Tantivy FTS search over episodic memories.
    pub fn search_episodic_fts(
        &self,
        query: &str,
        project_scope: &str,
    ) -> anyhow::Result<Vec<(EpisodicMemoryRecord, f64)>> {
        let column = self.active_fts_column()?;
        let fts_query = fts_query_for_column(column, query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, content, summary_of, session_id, project_scope,
                    importance, tier, metadata_json, memory_type,
                    created_at, updated_at, degraded_at, valid_until, superseded_by,
                    fts_score({column}, ?1) AS score
             FROM episodic_memory
             WHERE fts_match({column}, ?1) AND project_scope = ?2
             ORDER BY score DESC"
        );
        self.with_cached_stmt(&sql, |stmt| {
            bind_text(stmt, 1, &fts_query)?;
            bind_text(stmt, 2, project_scope)?;
            collect_rows_with_score(stmt, episodic_from_row)
        })
    }

    pub fn search_episodic_fts_top_k(
        &self,
        query: &str,
        project_scope: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(EpisodicMemoryRecord, f64)>> {
        if self.has_other_scope("episodic_memory", project_scope)? {
            let mut hits = self.search_episodic_fts(query, project_scope)?;
            hits.truncate(limit);
            return Ok(hits);
        }
        let column = self.active_fts_column()?;
        let fts_query = fts_query_for_column(column, query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT id, content, summary_of, session_id, project_scope,
                    importance, tier, metadata_json, memory_type,
                    created_at, updated_at, degraded_at, valid_until, superseded_by,
                    fts_score({column}, ?1) AS score
             FROM episodic_memory
             WHERE fts_match({column}, ?1)
             ORDER BY score DESC
             LIMIT ?2"
        );
        self.with_cached_stmt(&sql, |stmt| {
            bind_text(stmt, 1, &fts_query)?;
            bind_int(stmt, 2, limit.min(i64::MAX as usize) as i64)?;
            collect_rows_with_score(stmt, episodic_from_row)
        })
    }

    /// Count episodic_memory rows.
    pub fn count_episodic(&self) -> anyhow::Result<i64> {
        count_rows(&self.conn, "episodic_memory")
    }

    // -----------------------------------------------------------------------
    // facts / preferences / instructions / timeline
    // -----------------------------------------------------------------------

    pub fn insert_facts(&self, records: &[FactRecord]) -> anyhow::Result<()> {
        for record in records {
            let mut stmt = self.conn.prepare(
                "INSERT INTO facts (fact_id, subject, predicate, object, confidence,
                                    source_memory_id, session_id, project_scope, timestamp)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(fact_id) DO UPDATE SET
                    subject = excluded.subject, predicate = excluded.predicate,
                    object = excluded.object, confidence = excluded.confidence,
                    source_memory_id = excluded.source_memory_id,
                    session_id = excluded.session_id, project_scope = excluded.project_scope,
                    timestamp = excluded.timestamp",
            )?;
            bind_text(&mut stmt, 1, &record.fact_id)?;
            bind_text(&mut stmt, 2, &record.subject)?;
            bind_text(&mut stmt, 3, &record.predicate)?;
            bind_text(&mut stmt, 4, &record.object)?;
            bind_num(&mut stmt, 5, record.confidence)?;
            bind_opt_text(&mut stmt, 6, record.source_memory_id.as_deref())?;
            bind_text(&mut stmt, 7, &record.session_id)?;
            bind_text(&mut stmt, 8, &record.project_scope)?;
            bind_text(&mut stmt, 9, &record.timestamp)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    pub fn insert_preferences(&self, records: &[PreferenceRecord]) -> anyhow::Result<()> {
        for record in records {
            let mut stmt = self.conn.prepare(
                "INSERT INTO preferences (id, preference, topic, context_snippet,
                                         source_memory_id, session_id, project_scope, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    preference = excluded.preference, topic = excluded.topic,
                    context_snippet = excluded.context_snippet,
                    source_memory_id = excluded.source_memory_id,
                    session_id = excluded.session_id, project_scope = excluded.project_scope,
                    created_at = excluded.created_at",
            )?;
            bind_text(&mut stmt, 1, &record.id)?;
            bind_text(&mut stmt, 2, &record.preference)?;
            bind_opt_text(&mut stmt, 3, record.topic.as_deref())?;
            bind_opt_text(&mut stmt, 4, record.context_snippet.as_deref())?;
            bind_opt_text(&mut stmt, 5, record.source_memory_id.as_deref())?;
            bind_text(&mut stmt, 6, &record.session_id)?;
            bind_text(&mut stmt, 7, &record.project_scope)?;
            bind_text(&mut stmt, 8, &record.created_at)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    pub fn insert_instructions(&self, records: &[InstructionRecord]) -> anyhow::Result<()> {
        for record in records {
            let mut stmt = self.conn.prepare(
                "INSERT INTO instructions (id, instruction, topic, active, context_snippet,
                                          source_memory_id, session_id, project_scope, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                    instruction = excluded.instruction, topic = excluded.topic,
                    active = excluded.active, context_snippet = excluded.context_snippet,
                    source_memory_id = excluded.source_memory_id,
                    session_id = excluded.session_id, project_scope = excluded.project_scope,
                    created_at = excluded.created_at",
            )?;
            bind_text(&mut stmt, 1, &record.id)?;
            bind_text(&mut stmt, 2, &record.instruction)?;
            bind_opt_text(&mut stmt, 3, record.topic.as_deref())?;
            bind_int(&mut stmt, 4, record.active as i64)?;
            bind_opt_text(&mut stmt, 5, record.context_snippet.as_deref())?;
            bind_opt_text(&mut stmt, 6, record.source_memory_id.as_deref())?;
            bind_text(&mut stmt, 7, &record.session_id)?;
            bind_text(&mut stmt, 8, &record.project_scope)?;
            bind_text(&mut stmt, 9, &record.created_at)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    pub fn insert_timeline_events(&self, records: &[TimelineEventRecord]) -> anyhow::Result<()> {
        for record in records {
            let mut stmt = self.conn.prepare(
                "INSERT INTO timeline (event_id, date, description, source,
                                      source_memory_id, session_id, project_scope, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(event_id) DO UPDATE SET
                    date = excluded.date, description = excluded.description,
                    source = excluded.source, source_memory_id = excluded.source_memory_id,
                    session_id = excluded.session_id, project_scope = excluded.project_scope,
                    created_at = excluded.created_at",
            )?;
            bind_text(&mut stmt, 1, &record.event_id)?;
            bind_text(&mut stmt, 2, &record.date)?;
            bind_text(&mut stmt, 3, &record.description)?;
            bind_opt_text(&mut stmt, 4, record.source.as_deref())?;
            bind_opt_text(&mut stmt, 5, record.source_memory_id.as_deref())?;
            bind_text(&mut stmt, 6, &record.session_id)?;
            bind_text(&mut stmt, 7, &record.project_scope)?;
            bind_text(&mut stmt, 8, &record.created_at)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    /// Remove all structured rows produced by one working-memory turn before replaying extraction.
    pub fn delete_structured_by_source_memory(&self, memory_id: &str) -> anyhow::Result<()> {
        for table in ["facts", "preferences", "instructions", "timeline"] {
            let column = if table == "facts" {
                "fact_id"
            } else if table == "timeline" {
                "event_id"
            } else {
                "id"
            };
            let sql = format!(
                "DELETE FROM {table} WHERE source_memory_id = ?1 \
                 OR substr({column}, 1, length(?1) + 1) = (?1 || ':')"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            bind_text(&mut stmt, 1, memory_id)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    /// Expire episodic summaries containing a changed working row and reopen the whole source set.
    pub fn invalidate_episodic_for_working(
        &self,
        project_scope: &str,
        memory_id: &str,
        invalidated_at: &str,
    ) -> anyhow::Result<Vec<String>> {
        let mut invalidated = Vec::new();
        for episodic in self.list_episodic_by_project(project_scope)? {
            let ids = serde_json::from_str::<Vec<String>>(&episodic.summary_of)
                .unwrap_or_else(|_| episodic.summary_of.split(',').map(str::to_string).collect());
            if !ids.iter().any(|id| id == memory_id) {
                continue;
            }
            let mut update = self
                .conn
                .prepare("UPDATE episodic_memory SET valid_until = ?1 WHERE id = ?2")?;
            bind_text(&mut update, 1, invalidated_at)?;
            bind_text(&mut update, 2, &episodic.id)?;
            update.run_ignore_rows()?;
            for id in &ids {
                let mut reopen = self.conn.prepare(
                    "UPDATE working_memory
                     SET consolidated_at = NULL,
                         consolidation_lease_owner = NULL,
                         consolidation_lease_expires_at = NULL,
                         consolidation_claimed_at = NULL
                     WHERE id = ?1 AND project_scope = ?2",
                )?;
                bind_text(&mut reopen, 1, id)?;
                bind_text(&mut reopen, 2, project_scope)?;
                reopen.run_ignore_rows()?;
            }
            invalidated.push(episodic.id);
        }
        Ok(invalidated)
    }

    pub fn delete_embedding(&self, kind: MemoryKind, memory_id: &str) -> anyhow::Result<()> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM memory_embeddings WHERE memory_kind = ?1 AND memory_id = ?2")?;
        bind_text(&mut stmt, 1, kind.as_str())?;
        bind_text(&mut stmt, 2, memory_id)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    pub fn delete_embedding_for_model(
        &self,
        kind: MemoryKind,
        memory_id: &str,
        model: &str,
    ) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "DELETE FROM memory_embeddings WHERE memory_kind = ?1 AND memory_id = ?2 AND model = ?3",
        )?;
        bind_text(&mut stmt, 1, kind.as_str())?;
        bind_text(&mut stmt, 2, memory_id)?;
        bind_text(&mut stmt, 3, model)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    pub fn list_missing_embeddings(
        &self,
        kind: MemoryKind,
        model: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let (table, kind_name) = match kind {
            MemoryKind::Working => ("working_memory", "working"),
            MemoryKind::Episodic => ("episodic_memory", "episodic"),
            _ => return Ok(Vec::new()),
        };
        let sql = format!(
            "SELECT memory.id, memory.content FROM {table} AS memory
             LEFT JOIN memory_embeddings AS embedding
               ON embedding.memory_kind = '{kind_name}' AND embedding.memory_id = memory.id
                  AND embedding.model = ?1
             WHERE embedding.memory_id IS NULL AND trim(memory.content) != ''
             ORDER BY memory.created_at ASC, memory.id ASC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        bind_text(&mut stmt, 1, model)?;
        bind_int(&mut stmt, 2, limit.clamp(1, 10_000) as i64)?;
        stmt.run_collect_rows()?
            .iter()
            .map(|row| Ok((value_to_string(&row[0]), value_to_string(&row[1]))))
            .collect()
    }

    pub fn count_missing_embeddings(&self, kind: MemoryKind, model: &str) -> anyhow::Result<i64> {
        let (table, kind_name) = match kind {
            MemoryKind::Working => ("working_memory", "working"),
            MemoryKind::Episodic => ("episodic_memory", "episodic"),
            _ => return Ok(0),
        };
        let sql = format!(
            "SELECT COUNT(*) FROM {table} AS memory
             LEFT JOIN memory_embeddings AS embedding
               ON embedding.memory_kind = '{kind_name}' AND embedding.memory_id = memory.id
                  AND embedding.model = ?1
             WHERE embedding.memory_id IS NULL"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        bind_text(&mut stmt, 1, model)?;
        Ok(stmt
            .run_collect_rows()?
            .first()
            .map(|row| value_to_i64(&row[0]))
            .unwrap_or(0))
    }

    pub fn count_empty_missing_embeddings(&self, model: &str) -> anyhow::Result<i64> {
        let mut total = 0;
        for (table, kind_name) in [
            ("working_memory", "working"),
            ("episodic_memory", "episodic"),
        ] {
            let sql = format!(
                "SELECT COUNT(*) FROM {table} AS memory LEFT JOIN memory_embeddings AS embedding
                 ON embedding.memory_kind = '{kind_name}' AND embedding.memory_id = memory.id
                    AND embedding.model = ?1
                 WHERE embedding.memory_id IS NULL AND trim(memory.content) = ''"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            bind_text(&mut stmt, 1, model)?;
            total += stmt
                .run_collect_rows()?
                .first()
                .map(|row| value_to_i64(&row[0]))
                .unwrap_or(0);
        }
        Ok(total)
    }

    pub fn count_stale_model_embeddings(&self, model: &str) -> anyhow::Result<i64> {
        let mut stmt = self
            .conn
            .prepare("SELECT COUNT(*) FROM memory_embeddings WHERE model != ?1")?;
        bind_text(&mut stmt, 1, model)?;
        Ok(stmt
            .run_collect_rows()?
            .first()
            .map(|row| value_to_i64(&row[0]))
            .unwrap_or(0))
    }

    /// Count working/episodic rows per project (used by the stats command).
    pub fn count_working_by_project(&self, project_scope: &str) -> anyhow::Result<i64> {
        count_rows_where(&self.conn, "working_memory", "project_scope", project_scope)
    }

    pub fn count_episodic_by_project(&self, project_scope: &str) -> anyhow::Result<i64> {
        count_rows_where(
            &self.conn,
            "episodic_memory",
            "project_scope",
            project_scope,
        )
    }

    pub fn count_episodic_by_session(&self, session_id: &str) -> anyhow::Result<i64> {
        count_rows_where(&self.conn, "episodic_memory", "session_id", session_id)
    }

    /// Database file path (for diagnose).
    pub fn db_path(&self) -> &str {
        &self.path
    }

    /// LIKE search over structured tables (matches queryStructuredCandidates).
    /// project_scope filter: empty/legacy-unmarked rows stay visible for compatibility (isolation applies to newly written data).
    pub fn search_facts_like(
        &self,
        query: &str,
        project_scope: &str,
    ) -> anyhow::Result<Vec<FactRecord>> {
        let terms = crate::text::terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let predicates = terms
            .iter()
            .enumerate()
            .map(|(index, _)| {
                format!(
                    "(subject LIKE ?{p} ESCAPE '\\' OR predicate LIKE ?{p} ESCAPE '\\' OR object LIKE ?{p} ESCAPE '\\')",
                    p = index + 1
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let scope_bind = terms.len() + 1;
        let limit_bind = terms.len() + 2;
        let sql = format!(
            "SELECT fact_id, subject, predicate, object, confidence, source_memory_id, session_id, project_scope, timestamp
             FROM facts WHERE ({predicates})
               AND (project_scope = ?{scope_bind} OR project_scope = '' OR project_scope IS NULL)
             ORDER BY confidence DESC, timestamp DESC LIMIT ?{limit_bind}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        for (index, term) in terms.iter().enumerate() {
            bind_text(&mut stmt, index + 1, &like_contains_pattern(term))?;
        }
        bind_text(&mut stmt, scope_bind, project_scope)?;
        bind_int(&mut stmt, limit_bind, 30)?;
        collect_rows(&mut stmt, |row| {
            Ok(FactRecord {
                fact_id: row_to_string(row, 0)?,
                subject: row_to_string(row, 1)?,
                predicate: row_to_string(row, 2)?,
                object: row_to_string(row, 3)?,
                confidence: row_to_f64(row, 4)?,
                source_memory_id: row_to_opt_string(row, 5)?,
                session_id: row_to_string(row, 6)?,
                project_scope: row_to_string(row, 7)?,
                timestamp: row_to_string(row, 8)?,
            })
        })
    }

    pub fn search_instructions_like(
        &self,
        query: &str,
        project_scope: &str,
    ) -> anyhow::Result<Vec<InstructionRecord>> {
        let terms = crate::text::terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let predicates = term_predicates(&terms, &["instruction", "topic"]);
        let scope_bind = terms.len() + 1;
        let limit_bind = terms.len() + 2;
        let mut stmt = self.conn.prepare(format!(
            "SELECT id, instruction, topic, active, context_snippet, source_memory_id, session_id, project_scope, created_at
             FROM instructions WHERE ({predicates}) AND active = 1
               AND (project_scope = ?{scope_bind} OR project_scope = '' OR project_scope IS NULL)
             ORDER BY created_at DESC LIMIT ?{limit_bind}"
        ))?;
        bind_terms_and_scope(&mut stmt, &terms, scope_bind, project_scope)?;
        bind_int(&mut stmt, limit_bind, 30)?;
        collect_rows(&mut stmt, |row| {
            Ok(InstructionRecord {
                id: row_to_string(row, 0)?,
                instruction: row_to_string(row, 1)?,
                topic: row_to_opt_string(row, 2)?,
                active: row_to_i64(row, 3) != 0,
                context_snippet: row_to_opt_string(row, 4)?,
                source_memory_id: row_to_opt_string(row, 5)?,
                session_id: row_to_string(row, 6)?,
                project_scope: row_to_string(row, 7)?,
                created_at: row_to_string(row, 8)?,
            })
        })
    }

    pub fn search_preferences_like(
        &self,
        query: &str,
        project_scope: &str,
    ) -> anyhow::Result<Vec<PreferenceRecord>> {
        let terms = crate::text::terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let predicates = term_predicates(&terms, &["preference", "topic"]);
        let scope_bind = terms.len() + 1;
        let limit_bind = terms.len() + 2;
        let mut stmt = self.conn.prepare(format!(
            "SELECT id, preference, topic, context_snippet, source_memory_id, session_id, project_scope, created_at
             FROM preferences WHERE ({predicates})
               AND (project_scope = ?{scope_bind} OR project_scope = '' OR project_scope IS NULL)
             ORDER BY created_at DESC LIMIT ?{limit_bind}"
        ))?;
        bind_terms_and_scope(&mut stmt, &terms, scope_bind, project_scope)?;
        bind_int(&mut stmt, limit_bind, 30)?;
        collect_rows(&mut stmt, |row| {
            Ok(PreferenceRecord {
                id: row_to_string(row, 0)?,
                preference: row_to_string(row, 1)?,
                topic: row_to_opt_string(row, 2)?,
                context_snippet: row_to_opt_string(row, 3)?,
                source_memory_id: row_to_opt_string(row, 4)?,
                session_id: row_to_string(row, 5)?,
                project_scope: row_to_string(row, 6)?,
                created_at: row_to_string(row, 7)?,
            })
        })
    }

    pub fn search_timeline_like(
        &self,
        query: &str,
        project_scope: &str,
    ) -> anyhow::Result<Vec<TimelineEventRecord>> {
        let terms = crate::text::terms(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let predicates = term_predicates(&terms, &["date", "description"]);
        let scope_bind = terms.len() + 1;
        let limit_bind = terms.len() + 2;
        let mut stmt = self.conn.prepare(format!(
            "SELECT event_id, date, description, source, source_memory_id, session_id, project_scope, created_at
             FROM timeline WHERE ({predicates})
               AND (project_scope = ?{scope_bind} OR project_scope = '' OR project_scope IS NULL)
             ORDER BY created_at DESC LIMIT ?{limit_bind}"
        ))?;
        bind_terms_and_scope(&mut stmt, &terms, scope_bind, project_scope)?;
        bind_int(&mut stmt, limit_bind, 30)?;
        collect_rows(&mut stmt, |row| {
            Ok(TimelineEventRecord {
                event_id: row_to_string(row, 0)?,
                date: row_to_string(row, 1)?,
                description: row_to_string(row, 2)?,
                source: row_to_opt_string(row, 3)?,
                source_memory_id: row_to_opt_string(row, 4)?,
                session_id: row_to_string(row, 5)?,
                project_scope: row_to_string(row, 6)?,
                created_at: row_to_string(row, 7)?,
            })
        })
    }

    /// Structured memories by session (the facts/preferences/instructions/timeline count summary is used by diagnose).
    pub fn count_by_session(&self, table: &str, session_id: &str) -> anyhow::Result<i64> {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        bind_text(&mut stmt, 1, session_id)?;
        let rows = stmt.run_collect_rows()?;
        Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
    }

    // -----------------------------------------------------------------------
    // embeddings
    // -----------------------------------------------------------------------

    pub fn save_embedding(&self, record: &MemoryEmbeddingRecord) -> anyhow::Result<()> {
        anyhow::ensure!(!record.embedding.is_empty(), "embedding must not be empty");
        let active_model_id = crate::embedding_runtime::active_model_id();
        if record.model == active_model_id {
            anyhow::ensure!(
                record.embedding.len() == self.embedding_dim,
                "active-model embedding dimension mismatch: expected {}, got {}",
                self.embedding_dim,
                record.embedding.len()
            );
        }
        anyhow::ensure!(
            record
                .embedding
                .iter()
                .all(|value| value.is_finite() && (*value as f32).is_finite()),
            "embedding contains a non-finite or out-of-range value"
        );
        let mut stmt = self.conn.prepare(
            "INSERT INTO memory_embeddings (memory_kind, memory_id, embedding_json, model)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(memory_kind, memory_id, model) DO UPDATE SET
                embedding_json = excluded.embedding_json,
                created_at = CURRENT_TIMESTAMP",
        )?;
        bind_text(&mut stmt, 1, record.memory_kind.as_str())?;
        bind_text(&mut stmt, 2, &record.memory_id)?;
        bind_text(&mut stmt, 3, &serde_json::to_string(&record.embedding)?)?;
        bind_text(&mut stmt, 4, &record.model)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    pub fn get_embedding(
        &self,
        memory_kind: MemoryKind,
        memory_id: &str,
    ) -> anyhow::Result<Option<MemoryEmbeddingRecord>> {
        self.query_embedding(
            memory_kind,
            memory_id,
            Some(&crate::embedding_runtime::active_model_id()),
            false,
        )
    }

    pub fn get_embedding_for_model(
        &self,
        memory_kind: MemoryKind,
        memory_id: &str,
        model: &str,
    ) -> anyhow::Result<Option<MemoryEmbeddingRecord>> {
        self.query_embedding(memory_kind, memory_id, Some(model), true)
    }

    fn query_embedding(
        &self,
        memory_kind: MemoryKind,
        memory_id: &str,
        model: Option<&str>,
        strict_model: bool,
    ) -> anyhow::Result<Option<MemoryEmbeddingRecord>> {
        let sql = if strict_model {
            "SELECT memory_kind, memory_id, embedding_json, model
             FROM memory_embeddings WHERE memory_kind = ?1 AND memory_id = ?2 AND model = ?3"
        } else {
            "SELECT memory_kind, memory_id, embedding_json, model
             FROM memory_embeddings WHERE memory_kind = ?1 AND memory_id = ?2
             ORDER BY (model = ?3) DESC, created_at DESC LIMIT 1"
        };
        let mut stmt = self.conn.prepare(sql)?;
        bind_text(&mut stmt, 1, memory_kind.as_str())?;
        bind_text(&mut stmt, 2, memory_id)?;
        bind_text(&mut stmt, 3, model.unwrap_or_default())?;
        let mut found = None;
        let mut err = None;
        let mut parse_err = None;
        stmt.run_with_row_callback(|row| {
            match (
                row_to_string(row, 0),
                row_to_string(row, 1),
                row_to_string(row, 2),
                row_to_string(row, 3),
            ) {
                (Ok(kind), Ok(id), Ok(embedding_json), Ok(model)) => {
                    let embedding = match serde_json::from_str(&embedding_json) {
                        Ok(embedding) => embedding,
                        Err(error) => {
                            parse_err = Some(error.to_string());
                            return Ok(());
                        }
                    };
                    let memory_kind = match kind.as_str() {
                        "working" => MemoryKind::Working,
                        "episodic" => MemoryKind::Episodic,
                        _ => return Ok(()),
                    };
                    found = Some(MemoryEmbeddingRecord {
                        memory_kind,
                        memory_id: id,
                        embedding,
                        model,
                    });
                    Ok(())
                }
                (Err(e), _, _, _) => {
                    err = Some(e);
                    Ok(())
                }
                _ => Ok(()),
            }
        })?;
        if let Some(e) = err {
            return Err(e);
        }
        if let Some(error) = parse_err {
            anyhow::bail!("invalid stored embedding JSON: {error}");
        }
        Ok(found)
    }

    // -----------------------------------------------------------------------
    // session_state
    // -----------------------------------------------------------------------

    pub fn get_session_state(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Option<SessionStateRecord>> {
        self.with_cached_stmt(
            "SELECT session_id, project_scope, last_retained_user_turn, has_recalled_for_first_turn
             FROM session_state WHERE session_id = ?1",
            |stmt| {
                bind_text(stmt, 1, session_id)?;
                let mut found = None;
                let mut err = None;
                stmt.run_with_row_callback(|row| {
                    match (
                        row_to_string(row, 0),
                        row_to_string(row, 1),
                        row_to_i64(row, 2),
                        row_to_i64(row, 3),
                    ) {
                        (Ok(sid), Ok(scope), turn, recalled) => {
                            found = Some(SessionStateRecord {
                                session_id: sid,
                                project_scope: scope,
                                last_retained_user_turn: turn,
                                has_recalled_for_first_turn: recalled != 0,
                            });
                            Ok(())
                        }
                        (Err(e), _, _, _) => {
                            err = Some(e);
                            Ok(())
                        }
                        _ => Ok(()),
                    }
                })?;
                if let Some(e) = err {
                    return Err(e);
                }
                Ok(found)
            },
        )
    }

    pub fn save_session_state(&self, record: &SessionStateRecord) -> anyhow::Result<()> {
        self.with_cached_stmt(
            "INSERT INTO session_state (session_id, project_scope, last_retained_user_turn, has_recalled_for_first_turn)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                project_scope = excluded.project_scope,
                last_retained_user_turn = excluded.last_retained_user_turn,
                has_recalled_for_first_turn = excluded.has_recalled_for_first_turn,
                updated_at = CURRENT_TIMESTAMP",
            |stmt| {
                bind_text(stmt, 1, &record.session_id)?;
                bind_text(stmt, 2, &record.project_scope)?;
                bind_int(stmt, 3, record.last_retained_user_turn)?;
                bind_int(stmt, 4, record.has_recalled_for_first_turn as i64)?;
                stmt.run_ignore_rows()?;
                Ok(())
            },
        )
    }

    /// Update working_memory.consolidated_at (mark after consolidation).
    /// Note: reusing turso_core Statements risks leftover bindings, so each statement is prepared independently.
    pub fn mark_working_consolidated(
        &self,
        ids: &[String],
        consolidated_at: &str,
    ) -> anyhow::Result<()> {
        for id in ids {
            let mut stmt = self
                .conn
                .prepare("UPDATE working_memory SET consolidated_at = ?1 WHERE id = ?2")?;
            bind_text(&mut stmt, 1, consolidated_at)?;
            bind_text(&mut stmt, 2, id)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    pub fn claim_working_lease(
        &self,
        session_id: &str,
        project_scope: &str,
        owner: &str,
        ttl_ms: u64,
        limit: usize,
    ) -> anyhow::Result<(Vec<WorkingClaim>, usize)> {
        let now = chrono::Utc::now();
        let now_text = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let expires = (now + chrono::Duration::milliseconds(ttl_ms.min(i64::MAX as u64) as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<(Vec<WorkingClaim>, usize)> {
            let mut stmt = self.conn.prepare(
                "SELECT id, content, metadata_json FROM working_memory
                 WHERE session_id = ?1 AND project_scope = ?2 AND consolidated_at IS NULL
                   AND (consolidation_lease_owner IS NULL OR consolidation_lease_expires_at <= ?3)
                 ORDER BY created_at ASC, id ASC LIMIT ?4",
            )?;
            bind_text(&mut stmt, 1, session_id)?;
            bind_text(&mut stmt, 2, project_scope)?;
            bind_text(&mut stmt, 3, &now_text)?;
            bind_int(&mut stmt, 4, limit.min(i64::MAX as usize) as i64)?;
            let rows = stmt.run_collect_rows()?;
            let candidates_seen = rows.len();
            let mut claims = Vec::new();
            for row in rows {
                let text_at = |index: usize| -> String {
                    match row.get(index) {
                        Some(Value::Text(value)) => value.as_str().to_string(),
                        _ => String::new(),
                    }
                };
                let id = text_at(0);
                let content = text_at(1);
                let metadata =
                    serde_json::from_str::<serde_json::Value>(&text_at(2)).unwrap_or_default();
                let source_sha = metadata
                    .get("source_sha")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        use sha2::{Digest, Sha256};
                        format!("{:x}", Sha256::digest(content.as_bytes()))
                    });
                let mut update = self.conn.prepare(
                    "UPDATE working_memory SET consolidation_lease_owner = ?1,
                        consolidation_lease_expires_at = ?2, consolidation_claimed_at = ?3
                     WHERE id = ?4 AND project_scope = ?5 AND consolidated_at IS NULL
                       AND (consolidation_lease_owner IS NULL OR consolidation_lease_expires_at <= ?3)",
                )?;
                bind_text(&mut update, 1, owner)?;
                bind_text(&mut update, 2, &expires)?;
                bind_text(&mut update, 3, &now_text)?;
                bind_text(&mut update, 4, &id)?;
                bind_text(&mut update, 5, project_scope)?;
                update.run_ignore_rows()?;
                if self.conn.changes() == 1 {
                    claims.push(WorkingClaim {
                        id,
                        content,
                        source_sha,
                    });
                }
            }
            Ok((claims, candidates_seen))
        })();
        match result {
            Ok(claims) => match self.commit_tx() {
                Ok(()) => Ok(claims),
                Err(error) => {
                    let _ = self.rollback_tx();
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    pub fn heartbeat_working_lease(
        &self,
        ids: &[String],
        owner: &str,
        ttl_ms: u64,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now();
        let now_text = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let expires = (now + chrono::Duration::milliseconds(ttl_ms.min(i64::MAX as u64) as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for id in ids {
            let mut stmt = self.conn.prepare(
                "UPDATE working_memory SET consolidation_lease_expires_at = ?1
                 WHERE id = ?2 AND consolidation_lease_owner = ?3 AND consolidated_at IS NULL
                   AND consolidation_lease_expires_at > ?4",
            )?;
            bind_text(&mut stmt, 1, &expires)?;
            bind_text(&mut stmt, 2, id)?;
            bind_text(&mut stmt, 3, owner)?;
            bind_text(&mut stmt, 4, &now_text)?;
            stmt.run_ignore_rows()?;
            if self.conn.changes() != 1 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn get_working_claims(&self, owner: &str) -> anyhow::Result<Vec<WorkingClaim>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, metadata_json FROM working_memory
             WHERE consolidation_lease_owner = ?1 AND consolidated_at IS NULL
             ORDER BY created_at ASC, id ASC",
        )?;
        bind_text(&mut stmt, 1, owner)?;
        let mut claims = Vec::new();
        for row in stmt.run_collect_rows()? {
            let text = |index| match row.get(index) {
                Some(Value::Text(value)) => value.as_str().to_string(),
                _ => String::new(),
            };
            let content = text(1);
            let metadata = serde_json::from_str::<serde_json::Value>(&text(2)).unwrap_or_default();
            let source_sha = metadata
                .get("source_sha")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| {
                    use sha2::{Digest, Sha256};
                    format!("{:x}", Sha256::digest(content.as_bytes()))
                });
            claims.push(WorkingClaim {
                id: text(0),
                content,
                source_sha,
            });
        }
        Ok(claims)
    }

    pub fn release_working_lease(&self, ids: &[String], owner: &str) -> anyhow::Result<()> {
        for id in ids {
            let mut stmt = self.conn.prepare(
                "UPDATE working_memory SET consolidation_lease_owner = NULL,
                    consolidation_lease_expires_at = NULL, consolidation_claimed_at = NULL
                 WHERE id = ?1 AND consolidation_lease_owner = ?2 AND consolidated_at IS NULL",
            )?;
            bind_text(&mut stmt, 1, id)?;
            bind_text(&mut stmt, 2, owner)?;
            stmt.run_ignore_rows()?;
        }
        Ok(())
    }

    pub fn commit_working_lease(
        &self,
        claims: &[WorkingClaim],
        owner: &str,
        episodic: &EpisodicMemoryRecord,
    ) -> anyhow::Result<bool> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<bool> {
            for claim in claims {
                let mut stmt = self.conn.prepare(
                    "SELECT content, metadata_json, consolidation_lease_owner,
                            consolidation_lease_expires_at, consolidated_at
                     FROM working_memory WHERE id = ?1",
                )?;
                bind_text(&mut stmt, 1, &claim.id)?;
                let Some(row) = stmt.run_collect_rows()?.into_iter().next() else {
                    return Ok(false);
                };
                let text = |index| match row.get(index) {
                    Some(Value::Text(value)) => value.as_str().to_string(),
                    _ => String::new(),
                };
                let content = text(0);
                let metadata =
                    serde_json::from_str::<serde_json::Value>(&text(1)).unwrap_or_default();
                let lease_owner = text(2);
                let lease_expires = text(3);
                let consolidated = text(4);
                let current_sha = metadata
                    .get("source_sha")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        use sha2::{Digest, Sha256};
                        format!("{:x}", Sha256::digest(content.as_bytes()))
                    });
                if lease_owner != owner
                    || lease_expires <= now
                    || !consolidated.is_empty()
                    || current_sha != claim.source_sha
                {
                    return Ok(false);
                }
            }
            self.insert_episodic(episodic)?;
            for claim in claims {
                let mut stmt = self.conn.prepare(
                    "UPDATE working_memory SET consolidated_at = ?1,
                        consolidation_lease_owner = NULL, consolidation_lease_expires_at = NULL,
                        consolidation_claimed_at = NULL
                     WHERE id = ?2 AND consolidation_lease_owner = ?3 AND consolidated_at IS NULL",
                )?;
                bind_text(&mut stmt, 1, &now)?;
                bind_text(&mut stmt, 2, &claim.id)?;
                bind_text(&mut stmt, 3, owner)?;
                stmt.run_ignore_rows()?;
                if self.conn.changes() != 1 {
                    return Ok(false);
                }
            }
            Ok(true)
        })();
        match result {
            Ok(true) => {
                self.clear_consolidation_dirty_if_idle(&episodic.project_scope)?;
                self.commit_tx()?;
                Ok(true)
            }
            Ok(false) => {
                let _ = self.rollback_tx();
                let ids = claims
                    .iter()
                    .map(|claim| claim.id.clone())
                    .collect::<Vec<_>>();
                self.release_working_lease(&ids, owner)?;
                Ok(false)
            }
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    pub fn count_unconsolidated(
        &self,
        session_id: &str,
        project_scope: &str,
    ) -> anyhow::Result<i64> {
        let mut stmt = self.conn.prepare(
            "SELECT COUNT(*) FROM working_memory WHERE session_id = ?1 AND project_scope = ?2 AND consolidated_at IS NULL",
        )?;
        bind_text(&mut stmt, 1, session_id)?;
        bind_text(&mut stmt, 2, project_scope)?;
        let rows = stmt.run_collect_rows()?;
        Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
    }

    pub fn purge_expired(&self, project_scope: &str, now: &str) -> anyhow::Result<usize> {
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<usize> {
            const EXPIRED_PREDICATE: &str = "((valid_until IS NOT NULL AND trim(valid_until) != ''
                AND julianday(valid_until) <= julianday(?2))
                OR (superseded_by IS NOT NULL AND trim(superseded_by) != ''))";
            let mut removed = 0usize;
            for (table, kind) in [
                ("working_memory", MemoryKind::Working),
                ("episodic_memory", MemoryKind::Episodic),
            ] {
                let memory_kind = kind.as_str();
                let embedding_predicate = EXPIRED_PREDICATE.replace("?2", "?3");
                let delete_embeddings = format!(
                    "DELETE FROM memory_embeddings
                     WHERE memory_kind = ?1 AND memory_id IN (
                         SELECT id FROM {table} WHERE project_scope = ?2 AND {embedding_predicate}
                     )"
                );
                let mut embeddings = self.conn.prepare(&delete_embeddings)?;
                bind_text(&mut embeddings, 1, memory_kind)?;
                bind_text(&mut embeddings, 2, project_scope)?;
                bind_text(&mut embeddings, 3, now)?;
                embeddings.run_ignore_rows()?;

                let delete_memories =
                    format!("DELETE FROM {table} WHERE project_scope = ?1 AND {EXPIRED_PREDICATE}");
                let mut memories = self.conn.prepare(&delete_memories)?;
                bind_text(&mut memories, 1, project_scope)?;
                bind_text(&mut memories, 2, now)?;
                memories.run_ignore_rows()?;
                removed += self.conn.changes().max(0) as usize;
            }
            Ok(removed)
        })();
        match result {
            Ok(removed) => match self.commit_tx() {
                Ok(()) => Ok(removed),
                Err(error) => {
                    let _ = self.rollback_tx();
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    /// Remove durable vectors whose source memory no longer exists. The vector index is rebuilt
    /// by the caller after cleanup so stale IDs disappear from both layers together.
    pub fn purge_orphan_embeddings(&self) -> anyhow::Result<usize> {
        let count = |storage: &Self| -> anyhow::Result<i64> {
            let mut stmt = storage
                .conn
                .prepare("SELECT COUNT(*) FROM memory_embeddings")?;
            let rows = stmt.run_collect_rows()?;
            Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
        };
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<usize> {
            let before = count(self)?;
            self.conn.execute(
                "DELETE FROM memory_embeddings
                 WHERE memory_kind NOT IN ('working', 'episodic')
                    OR (memory_kind = 'working' AND NOT EXISTS (
                        SELECT 1 FROM working_memory WHERE working_memory.id = memory_embeddings.memory_id
                    ))
                    OR (memory_kind = 'episodic' AND NOT EXISTS (
                        SELECT 1 FROM episodic_memory WHERE episodic_memory.id = memory_embeddings.memory_id
                    ))",
            )?;
            let active_model_id = crate::embedding_runtime::active_model_id();
            let active_dimension = crate::model_manifest::configured_dimension()?;
            let invalid_vectors = self
                .list_all_embeddings()?
                .into_iter()
                .filter(|embedding| {
                    (embedding.model == active_model_id
                        && embedding.embedding.len() != active_dimension)
                        || embedding.embedding.is_empty()
                        || !embedding
                            .embedding
                            .iter()
                            .all(|value| value.is_finite() && (*value as f32).is_finite())
                })
                .map(|embedding| (embedding.memory_kind, embedding.memory_id, embedding.model))
                .collect::<Vec<_>>();
            for (kind, memory_id, model) in invalid_vectors {
                self.delete_embedding_for_model(kind, &memory_id, &model)?;
            }
            let after = count(self)?;
            Ok(before.saturating_sub(after).max(0) as usize)
        })();
        match result {
            Ok(removed) => match self.commit_tx() {
                Ok(()) => Ok(removed),
                Err(error) => {
                    let _ = self.rollback_tx();
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    /// Enumerate logical scopes stored in this database, including legacy multi-scope data.
    pub fn list_project_scopes(&self) -> anyhow::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT project_scope FROM working_memory
             UNION SELECT project_scope FROM episodic_memory
             UNION SELECT project_scope FROM facts
             UNION SELECT project_scope FROM preferences
             UNION SELECT project_scope FROM instructions
             UNION SELECT project_scope FROM timeline
             UNION SELECT project_scope FROM session_state
             UNION SELECT project_scope FROM pending_turns",
        )?;
        let mut scopes = stmt
            .run_collect_rows()?
            .iter()
            .map(|row| value_to_string(&row[0]))
            .collect::<Vec<_>>();
        scopes.sort();
        Ok(scopes)
    }

    /// Delete working/episodic memories for a project (used by the clear command).
    pub fn clear_project_memories(&self, project_scope: &str) -> anyhow::Result<()> {
        self.begin_tx()?;
        let result = (|| -> anyhow::Result<()> {
            // Delete vectors by their owning memory rows before removing those rows.
            let mut embeddings = self.conn.prepare(
                "DELETE FROM memory_embeddings
                 WHERE (memory_kind = 'working' AND memory_id IN
                        (SELECT id FROM working_memory WHERE project_scope = ?1))
                    OR (memory_kind = 'episodic' AND memory_id IN
                        (SELECT id FROM episodic_memory WHERE project_scope = ?1))",
            )?;
            bind_text(&mut embeddings, 1, project_scope)?;
            embeddings.run_ignore_rows()?;
            for table in [
                "working_memory",
                "episodic_memory",
                "facts",
                "preferences",
                "instructions",
                "timeline",
                "session_state",
                "pending_turns",
            ] {
                let sql = format!("DELETE FROM {table} WHERE project_scope = ?1");
                let mut stmt = self.conn.prepare(&sql)?;
                bind_text(&mut stmt, 1, project_scope)?;
                stmt.run_ignore_rows()?;
            }
            self.set_meta("fts_dirty", "true")?;
            self.set_meta("needs_rehydrate", "true")?;
            self.set_meta("index_state", "partial")?;
            Ok(())
        })();
        match result {
            Ok(()) => self.commit_tx(),
            Err(error) => {
                let _ = self.rollback_tx();
                Err(error)
            }
        }
    }

    // -----------------------------------------------------------------------
    // FTS rebuild (/memory rebuild semantics)
    // -----------------------------------------------------------------------

    /// Drop the FTS index (avoids per-row commits during bulk fill; rebuild_fts rebuilds it once afterwards).
    pub fn drop_fts(&self) -> anyhow::Result<()> {
        self.conn.execute("DROP INDEX IF EXISTS fts_working_idx")?;
        self.conn.execute("DROP INDEX IF EXISTS fts_episodes_idx")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_working_idx_v2")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_episodes_idx_v2")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_working_search_idx")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_episodes_search_idx")?;
        Ok(())
    }

    /// Rebuild the FTS index under the migration lease while keeping the content index live.
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_working_idx ON working_memory USING fts (content)",
        )?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_episodes_idx ON episodic_memory USING fts (content)",
        )?;
        let owner = format!(
            "rebuild:{}:{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let token = self
            .acquire_fts_lease(&owner)?
            .ok_or_else(|| anyhow::anyhow!("FTS migration is already owned by another server"))?;
        self.begin_tx()?;
        let fallback_result = (|| -> anyhow::Result<()> {
            self.renew_fts_lease_in_tx(&owner, token)?;
            self.set_meta("fts_state", "content")?;
            self.set_meta("fts_version", "1")?;
            self.set_meta("fts_phase", "backfilled")?;
            self.set_meta("fts_dirty", "true")?;
            self.set_meta("index_state", "partial")
        })();
        match fallback_result {
            Ok(()) => self.commit_tx()?,
            Err(error) => {
                let _ = self.rollback_tx();
                let _ = self.release_fts_lease(&owner, token);
                return Err(error);
            }
        }
        self.conn
            .execute("DROP INDEX IF EXISTS fts_working_idx_v2")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_episodes_idx_v2")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_working_search_idx")?;
        self.conn
            .execute("DROP INDEX IF EXISTS fts_episodes_search_idx")?;
        match self.migrate_search_text_under_lease(&owner, token) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.record_fts_migration_error(&owner, token, &error.to_string());
                let _ = self.release_fts_lease(&owner, token);
                Err(error)
            }
        }
    }
}

impl Storage for TursoStorage {
    fn init(&mut self) -> anyhow::Result<()> {
        TursoStorage::init(self)
    }

    fn diagnose(&self) -> anyhow::Result<serde_json::Value> {
        let working = self.count_working()?;
        let episodic = self.count_episodic()?;
        let unconsolidated_working =
            count_rows_where_null(&self.conn, "working_memory", "consolidated_at")?;
        let degraded_episodic =
            count_rows_where_not_null(&self.conn, "episodic_memory", "degraded_at")?;
        let scope_distribution = self.scope_distribution()?;
        let pending_turns = count_rows_where_not_done(&self.conn)?;
        let failed_turns = count_rows_where(&self.conn, "pending_turns", "status", "failed")?;
        let fts_dirty = self.get_meta("fts_dirty")?.as_deref() == Some("true");
        let needs_rehydrate = self.get_meta("needs_rehydrate")?.as_deref() == Some("true");
        let index_state = self
            .get_meta("index_state")?
            .unwrap_or_else(|| "ok".to_string());
        let rehydrate_last_error = self.get_meta("rehydrate_last_error")?;
        let fts_state = self
            .get_meta("fts_state")?
            .unwrap_or_else(|| "content".into());
        let fts_phase = self.get_meta("fts_phase")?.unwrap_or_else(|| "idle".into());
        let fts_version = self.get_meta("fts_version")?.unwrap_or_else(|| "1".into());
        let fts_last_error = self.get_meta("fts_last_error")?;
        let fts_null_rows = self.count_null_search_text()?;
        let fts_working_v1 = self.fts_index_exists("fts_working_idx")?;
        let fts_episodes_v1 = self.fts_index_exists("fts_episodes_idx")?;
        let fts_working_v2 = self.fts_index_exists("fts_working_idx_v2")?;
        let fts_episodes_v2 = self.fts_index_exists("fts_episodes_idx_v2")?;
        let fts_search_text_probe_ok = if fts_state == "search_text"
            && fts_working_v2
            && fts_episodes_v2
            && fts_null_rows == 0
        {
            self.probe_search_text_indexes().is_ok()
        } else {
            false
        };
        let fts_invariant_ok = match fts_state.as_str() {
            "search_text" => {
                fts_null_rows == 0 && fts_working_v2 && fts_episodes_v2 && fts_search_text_probe_ok
            }
            "content" => fts_working_v1 && fts_episodes_v1,
            _ => false,
        };
        let model_verify_disabled = crate::model_manifest::hash_verification_disabled();
        let model_verify_allowed = crate::model_manifest::hash_verification_disable_allowed();
        let embedding_model = self
            .get_meta("embedding_model")?
            .unwrap_or_else(crate::embedding_runtime::active_model_id);
        let model_verify_status = if model_verify_disabled && !model_verify_allowed {
            "rejected"
        } else if model_verify_disabled {
            "disabled"
        } else {
            "enabled"
        };
        let lock_rows = self
            .conn
            .prepare(
                "SELECT owner, expires_at, fencing_token FROM fts_migration_lock WHERE id = 1",
            )?
            .run_collect_rows()?;
        let fts_lock = lock_rows.first().map(|row| {
            serde_json::json!({
                "owner": value_to_opt_string(&row[0]),
                "expires_at": value_to_opt_string(&row[1]),
                "fencing_token": value_to_i64(&row[2]),
            })
        });
        // 完整性检查（PRAGMA integrity_check：返回 "ok" 或损坏详情）。
        // 手动触发可接受慢查询；失败不代表库损坏（可能只是被占用），错误会如实上报。
        // integrity_check 无错误时返回 NULL（或 "ok"），有错误时返回错误详情文本
        let integrity = self
            .conn
            .prepare("PRAGMA integrity_check")
            .and_then(|mut stmt| stmt.run_collect_rows());
        let integrity_status = match &integrity {
            Ok(rows) => {
                let texts: Vec<String> = rows
                    .iter()
                    .map(|row| match &row[0] {
                        Value::Text(text) => text.as_str().to_string(),
                        _ => String::new(),
                    })
                    .collect();
                if texts.is_empty()
                    || texts.iter().all(|t| t.is_empty())
                    || texts.iter().any(|t| t == "ok")
                {
                    "ok"
                } else {
                    "corrupt"
                }
            }
            Err(_) => "check-failed",
        };
        Ok(serde_json::json!({
            "backend": "turso",
            "path": self.path,
            "working_memory_count": working,
            "unconsolidated_working_count": unconsolidated_working,
            "episodic_memory_count": episodic,
            "degraded_episodic_count": degraded_episodic,
            "degraded_episodic_pct": if episodic == 0 { 0.0 } else { degraded_episodic as f64 * 100.0 / episodic as f64 },
            "scope_distribution": scope_distribution,
            "pending_turn_count": pending_turns,
            "failed_turn_count": failed_turns,
            "fts_dirty": fts_dirty,
            "needs_rehydrate": needs_rehydrate,
            "index_state": index_state,
            "rehydrate_last_error": rehydrate_last_error,
            "fts_state": fts_state,
            "fts_phase": fts_phase,
            "fts_version": fts_version,
            "fts_last_error": fts_last_error,
            "fts_search_text_null_rows": fts_null_rows,
            "fts_indexes": {
                "working_v1": fts_working_v1,
                "episodic_v1": fts_episodes_v1,
                "working_v2": fts_working_v2,
                "episodic_v2": fts_episodes_v2,
            },
            "fts_search_text_probe_ok": fts_search_text_probe_ok,
            "fts_invariant_ok": fts_invariant_ok,
            "fts_migration_lock": fts_lock,
            "model_verify": model_verify_status,
            "embedding_model": embedding_model,
            "embedding_dimension": self.embedding_dim,
            "unsafe_model": model_verify_disabled && model_verify_allowed,
            "model_verify_error": if model_verify_disabled && !model_verify_allowed {
                Some("PI_MEMORY_MODEL_VERIFY=0 is only allowed in debug or PI_MEMORY_ENV=development builds")
            } else {
                None
            },
            "fts": "tantivy",
            "integrity": integrity_status,
        }))
    }
}

impl TursoStorage {
    fn scope_distribution(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT memory_table, project_scope, COUNT(*) FROM (
                 SELECT 'working_memory' AS memory_table, project_scope FROM working_memory
                 UNION ALL SELECT 'episodic_memory', project_scope FROM episodic_memory
                 UNION ALL SELECT 'facts', project_scope FROM facts
                 UNION ALL SELECT 'preferences', project_scope FROM preferences
                 UNION ALL SELECT 'instructions', project_scope FROM instructions
                 UNION ALL SELECT 'timeline', project_scope FROM timeline
             ) GROUP BY memory_table, project_scope
             ORDER BY memory_table, project_scope",
        )?;
        stmt.run_collect_rows()?
            .iter()
            .map(|row| {
                Ok(serde_json::json!({
                    "table": value_to_string(&row[0]),
                    "projectScope": value_to_string(&row[1]),
                    "count": value_to_i64(&row[2]),
                }))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Row deserialization
// ---------------------------------------------------------------------------

fn working_from_row(row: &Row) -> anyhow::Result<WorkingMemoryRecord> {
    Ok(WorkingMemoryRecord {
        id: row_to_string(row, 0)?,
        content: row_to_string(row, 1)?,
        embed_text: row_to_opt_string(row, 2)?,
        source: row_to_opt_string(row, 3)?,
        session_id: row_to_string(row, 4)?,
        project_scope: row_to_string(row, 5)?,
        importance: row_to_f64(row, 6)?,
        memory_type: row_to_string(row, 7)?,
        metadata: metadata_from_json(row_to_string(row, 8)?),
        created_at: row_to_string(row, 9)?,
        updated_at: row_to_string(row, 10)?,
        consolidated_at: row_to_opt_string(row, 11)?,
        valid_until: row_to_opt_string(row, 12)?,
        superseded_by: row_to_opt_string(row, 13)?,
    })
}

fn episodic_from_row(row: &Row) -> anyhow::Result<EpisodicMemoryRecord> {
    Ok(EpisodicMemoryRecord {
        id: row_to_string(row, 0)?,
        content: row_to_string(row, 1)?,
        summary_of: row_to_string(row, 2)?,
        session_id: row_to_string(row, 3)?,
        project_scope: row_to_string(row, 4)?,
        importance: row_to_f64(row, 5)?,
        tier: row_to_i64(row, 6),
        memory_type: row_to_string(row, 7)?,
        metadata: metadata_from_json(row_to_string(row, 8)?),
        created_at: row_to_string(row, 9)?,
        updated_at: row_to_string(row, 10)?,
        degraded_at: row_to_opt_string(row, 11)?,
        valid_until: row_to_opt_string(row, 12)?,
        superseded_by: row_to_opt_string(row, 13)?,
    })
}

fn metadata_from_json(raw: String) -> Option<serde_json::Value> {
    if raw.is_empty() {
        return None;
    }
    serde_json::from_str(&raw).ok()
}

fn metadata_to_json(metadata: Option<&serde_json::Value>) -> String {
    metadata
        .map(|m| serde_json::to_string(m).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| "{}".into())
}

// ---------------------------------------------------------------------------
// Collection helpers
// ---------------------------------------------------------------------------

/// Fetch a single row (None when absent).
fn collect_first_row<T>(
    stmt: &mut Statement,
    parse: fn(&Row) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    let mut found = None;
    let mut err = None;
    stmt.run_with_row_callback(|row| match parse(row) {
        Ok(value) => {
            found = Some(value);
            Ok(())
        }
        Err(e) => {
            err = Some(e);
            Ok(())
        }
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(found)
}

/// Collect all rows.
fn collect_rows<T>(
    stmt: &mut Statement,
    parse: fn(&Row) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<T>> {
    let mut results = Vec::new();
    let mut err = None;
    stmt.run_with_row_callback(|row| match parse(row) {
        Ok(value) => {
            results.push(value);
            Ok(())
        }
        Err(e) => {
            err = Some(e);
            Ok(())
        }
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(results)
}

/// Collect all rows with the score column (index 14) attached.
fn collect_rows_with_score<T>(
    stmt: &mut Statement,
    parse: fn(&Row) -> anyhow::Result<T>,
) -> anyhow::Result<Vec<(T, f64)>> {
    let mut results = Vec::new();
    let mut err = None;
    stmt.run_with_row_callback(|row| match parse(row) {
        Ok(value) => {
            let score = row_to_f64(row, 14).unwrap_or(0.0);
            results.push((value, score));
            Ok(())
        }
        Err(e) => {
            err = Some(e);
            Ok(())
        }
    })?;
    if let Some(e) = err {
        return Err(e);
    }
    Ok(results)
}

fn count_rows(conn: &Arc<Connection>, table: &str) -> anyhow::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let rows = conn.prepare(&sql)?.run_collect_rows()?;
    Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
}

fn count_rows_where(
    conn: &Arc<Connection>,
    table: &str,
    column: &str,
    value: &str,
) -> anyhow::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1");
    let mut stmt = conn.prepare(&sql)?;
    bind_text(&mut stmt, 1, value)?;
    let rows = stmt.run_collect_rows()?;
    Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
}

fn count_rows_where_not_null(
    conn: &Arc<Connection>,
    table: &str,
    column: &str,
) -> anyhow::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} IS NOT NULL");
    let rows = conn.prepare(&sql)?.run_collect_rows()?;
    Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
}

fn count_rows_where_null(conn: &Arc<Connection>, table: &str, column: &str) -> anyhow::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} IS NULL");
    let rows = conn.prepare(&sql)?.run_collect_rows()?;
    Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
}

fn count_rows_where_not_done(conn: &Arc<Connection>) -> anyhow::Result<i64> {
    let rows = conn
        .prepare("SELECT COUNT(*) FROM pending_turns WHERE status != 'done'")?
        .run_collect_rows()?;
    Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Value binding and extraction helpers (thin wrappers over the turso_core low-level API)
// ---------------------------------------------------------------------------

fn active_fts_column_for_mode(
    configured_mode: Option<&str>,
    persisted_state: Option<&str>,
) -> anyhow::Result<&'static str> {
    match configured_mode {
        Some("content") => Ok("content"),
        Some("search_text") | None => Ok(if persisted_state == Some("search_text") {
            "search_text"
        } else {
            "content"
        }),
        Some("ngram") => {
            anyhow::bail!("PI_MEMORY_FTS_MODE=ngram is not supported; use search_text or content")
        }
        Some(mode) => {
            anyhow::bail!("unsupported PI_MEMORY_FTS_MODE={mode}; expected search_text or content")
        }
    }
}

fn bind_text(stmt: &mut Statement, index: usize, value: &str) -> anyhow::Result<()> {
    stmt.bind_at(
        NonZeroUsize::new(index).expect("index must be non-zero"),
        Value::Text(Text::new(value.to_string())),
    )?;
    Ok(())
}

fn bind_opt_text(stmt: &mut Statement, index: usize, value: Option<&str>) -> anyhow::Result<()> {
    match value {
        Some(text) => bind_text(stmt, index, text),
        None => {
            stmt.bind_at(
                NonZeroUsize::new(index).expect("index must be non-zero"),
                Value::Null,
            )?;
            Ok(())
        }
    }
}

fn bind_int(stmt: &mut Statement, index: usize, value: i64) -> anyhow::Result<()> {
    stmt.bind_at(
        NonZeroUsize::new(index).expect("index must be non-zero"),
        Value::Numeric(Numeric::Integer(value)),
    )?;
    Ok(())
}

fn bind_num(stmt: &mut Statement, index: usize, value: f64) -> anyhow::Result<()> {
    // turso_core Numeric has no From<f64>; build via NonNan; NaN binds as NULL
    let numeric = match turso_core::NonNan::new(value) {
        Some(non_nan) => Numeric::Float(non_nan),
        None => return Ok(()), // NaN binds as NULL
    };
    stmt.bind_at(
        NonZeroUsize::new(index).expect("index must be non-zero"),
        Value::Numeric(numeric),
    )?;
    Ok(())
}

fn term_predicates(terms: &[String], columns: &[&str]) -> String {
    terms
        .iter()
        .enumerate()
        .map(|(index, _)| {
            let bind = index + 1;
            let fields = columns
                .iter()
                .map(|column| format!("{column} LIKE ?{bind} ESCAPE '\\'"))
                .collect::<Vec<_>>()
                .join(" OR ");
            format!("({fields})")
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn bind_terms_and_scope(
    stmt: &mut Statement,
    terms: &[String],
    scope_bind: usize,
    project_scope: &str,
) -> anyhow::Result<()> {
    for (index, term) in terms.iter().enumerate() {
        bind_text(stmt, index + 1, &like_contains_pattern(term))?;
    }
    bind_text(stmt, scope_bind, project_scope)
}

fn like_contains_pattern(term: &str) -> String {
    let escaped = term
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

fn row_to_string(row: &Row, index: usize) -> anyhow::Result<String> {
    let value = row.get_value(index);
    match value {
        Value::Null => Ok(String::new()),
        Value::Text(text) => Ok(text.as_str().to_string()),
        Value::Numeric(Numeric::Integer(i)) => Ok(i.to_string()),
        Value::Numeric(Numeric::Float(f)) => Ok(Numeric::Float(*f).to_f64().to_string()),
        Value::Blob(bytes) => Ok(String::from_utf8_lossy(bytes).into_owned()),
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Text(text) => text.as_str().to_string(),
        Value::Numeric(Numeric::Integer(value)) => value.to_string(),
        Value::Numeric(Numeric::Float(value)) => Numeric::Float(*value).to_f64().to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn value_to_opt_string(value: &Value) -> Option<String> {
    (!matches!(value, Value::Null)).then(|| value_to_string(value))
}

fn row_to_opt_string(row: &Row, index: usize) -> anyhow::Result<Option<String>> {
    match row.get_value(index) {
        Value::Null => Ok(None),
        _ => Ok(Some(row_to_string(row, index)?)),
    }
}

fn row_to_f64(row: &Row, index: usize) -> anyhow::Result<f64> {
    Ok(value_to_f64(row.get_value(index)))
}

fn value_to_f64(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Numeric(n) => n.to_f64(),
        Value::Text(text) => text.as_str().parse().unwrap_or(0.0),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).parse().unwrap_or(0.0),
    }
}

fn row_to_i64(row: &Row, index: usize) -> i64 {
    match row.get_value(index) {
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => Numeric::Float(*f).to_f64() as i64,
        _ => 0,
    }
}

fn value_to_i64(value: &Value) -> i64 {
    match value {
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => Numeric::Float(*f).to_f64() as i64,
        _ => 0,
    }
}

fn fts_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn fts_lease_expiry() -> String {
    let lease_ms = fts_lease_duration_ms();
    (chrono::Utc::now() + chrono::Duration::milliseconds(lease_ms as i64))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn fts_lease_duration_ms() -> u64 {
    const DEFAULT_LEASE_MS: u64 = 30_000;
    std::env::var("PI_MEMORY_FTS_MIGRATION_LEASE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_LEASE_MS)
        .clamp(3_000, 600_000)
}

fn fts_lease_heartbeat_interval() -> std::time::Duration {
    std::time::Duration::from_millis((fts_lease_duration_ms() / 3).clamp(1_000, 10_000))
}

fn fts_query_for_column(column: &str, query: &str) -> String {
    if column == "search_text" {
        crate::text::to_fts_query(query)
    } else {
        query.to_string()
    }
}

#[cfg(test)]
fn pause_fts_migration_at_phase_for_test(phase: &str, batch_number: usize) -> anyhow::Result<()> {
    if std::env::var("PI_MEMORY_TEST_FTS_PAUSE_PHASE").as_deref() != Ok(phase) {
        return Ok(());
    }
    if phase == "backfill_batch" {
        let pause_after_batch = std::env::var("PI_MEMORY_TEST_FTS_PAUSE_AFTER_BATCH")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(1);
        if batch_number != pause_after_batch {
            return Ok(());
        }
    }
    let Ok(ready_path) = std::env::var("PI_MEMORY_TEST_FTS_READY_FILE") else {
        return Ok(());
    };
    let release_path = std::env::var("PI_MEMORY_TEST_FTS_RELEASE_FILE")?;
    std::fs::write(ready_path, phase)?;
    while !std::path::Path::new(&release_path).exists() {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
fn delay_fts_migration_row_for_test() {
    let delay_ms = std::env::var("PI_MEMORY_TEST_FTS_ROW_DELAY_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    if delay_ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
    }
}

#[cfg(test)]
mod fts_lease_tests {
    use super::{active_fts_column_for_mode, TursoStorage};
    use crate::models::WorkingMemoryRecord;

    #[test]
    fn fts_mode_rejects_unimplemented_ngram_and_unknown_values() {
        assert_eq!(
            active_fts_column_for_mode(None, Some("search_text")).unwrap(),
            "search_text"
        );
        assert_eq!(
            active_fts_column_for_mode(Some("content"), Some("search_text")).unwrap(),
            "content"
        );
        assert_eq!(
            active_fts_column_for_mode(Some("search_text"), Some("content")).unwrap(),
            "content"
        );
        assert!(active_fts_column_for_mode(Some("ngram"), None)
            .unwrap_err()
            .to_string()
            .contains("not supported"));
        assert!(active_fts_column_for_mode(Some("invalid"), None).is_err());
    }

    #[test]
    fn ngram_fts_spike_matches_basic_cjk_and_english_queries() -> anyhow::Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pi-memory-fts-ngram-{nonce}.db"));
        let path = path.to_string_lossy().into_owned();
        let storage = TursoStorage::open(&path)?;
        storage.conn.execute("DROP INDEX fts_working_idx_v2")?;
        storage.conn.execute(
            "CREATE INDEX fts_working_idx_v2 ON working_memory USING fts (search_text)
             WITH (tokenizer='ngram')",
        )?;
        storage.set_meta("fts_state", "search_text")?;
        storage.insert_working(&WorkingMemoryRecord {
            id: "ngram-spike".into(),
            content: "项目使用向量索引和 vector search".into(),
            embed_text: None,
            source: None,
            session_id: "ngram-spike".into(),
            project_scope: "/ngram-spike".into(),
            importance: 0.5,
            memory_type: "working".into(),
            metadata: None,
            created_at: "2026-11-23T00:00:00.000Z".into(),
            updated_at: "2026-11-23T00:00:00.000Z".into(),
            consolidated_at: None,
            valid_until: None,
            superseded_by: None,
        })?;

        let cjk = storage.search_working_fts_top_k("向量", "/ngram-spike", 10)?;
        let english = storage.search_working_fts_top_k("vector", "/ngram-spike", 10)?;
        eprintln!(
            "ngram FTS spike: CJK hits={}, English hits={}",
            cjk.len(),
            english.len()
        );
        assert!(!cjk.is_empty(), "ngram should retain the CJK bigram hit");
        assert_eq!(english.len(), 1, "English word should remain searchable");
        Ok(())
    }
    use std::process::{Child, Command, Stdio};

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn expired_fts_leader_is_fenced_after_takeover() -> anyhow::Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pi-memory-fts-fence-{nonce}.db"));
        let path = path.to_string_lossy().into_owned();
        let storage = TursoStorage::open_deferred(&path)?;
        let observer = TursoStorage::open_deferred(&path)?;

        let old_token = storage
            .acquire_fts_lease("old-leader")?
            .expect("first leader acquires the lease");
        assert!(
            observer.acquire_fts_lease("competing-leader")?.is_none(),
            "a live lease has exactly one leader"
        );
        storage.begin_tx()?;
        storage.conn.execute(
            "UPDATE fts_migration_lock SET expires_at = '2000-01-01T00:00:00Z'
             WHERE id = 1 AND owner = 'old-leader'",
        )?;
        storage.commit_tx()?;

        storage.begin_tx()?;
        storage.renew_fts_lease_in_tx("old-leader", old_token)?;
        storage.commit_tx()?;
        assert!(
            observer.acquire_fts_lease("competing-leader")?.is_none(),
            "the unchanged owner/token can renew before a contender takes over"
        );

        storage.begin_tx()?;
        storage.conn.execute(
            "UPDATE fts_migration_lock SET expires_at = '2000-01-01T00:00:00Z'
             WHERE id = 1 AND owner = 'old-leader'",
        )?;
        storage.commit_tx()?;

        let new_token = observer
            .acquire_fts_lease("new-leader")?
            .expect("expired lease is taken over");
        assert!(new_token > old_token, "takeover increments fencing token");

        storage.begin_tx()?;
        assert!(storage
            .renew_fts_lease_in_tx("old-leader", old_token)
            .is_err());
        storage.rollback_tx()?;
        assert!(storage.release_fts_lease("old-leader", old_token).is_err());
        observer.assert_fts_lease("new-leader", new_token)?;

        observer.release_fts_lease("new-leader", new_token)?;
        drop(observer);
        drop(storage);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
        Ok(())
    }

    #[test]
    fn non_leader_keeps_serving_the_content_index() -> anyhow::Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pi-memory-fts-reader-{nonce}.db"));
        let path = path.to_string_lossy().into_owned();
        let leader = TursoStorage::open_deferred(&path)?;
        let reader = TursoStorage::open_deferred(&path)?;
        leader.insert_working(&WorkingMemoryRecord {
            id: "legacy-row".into(),
            content: "legacy content remains searchable during migration".into(),
            embed_text: None,
            source: None,
            session_id: "session".into(),
            project_scope: "/project".into(),
            importance: 0.5,
            memory_type: "working".into(),
            metadata: None,
            created_at: "2026-08-21T00:00:00Z".into(),
            updated_at: "2026-08-21T00:00:00Z".into(),
            consolidated_at: None,
            valid_until: None,
            superseded_by: None,
        })?;

        let token = leader
            .acquire_fts_lease("migration-leader")?
            .expect("leader claims migration");
        assert!(!reader.migrate_search_text_if_needed()?);
        assert_eq!(
            reader
                .search_working_fts("legacy", "/project")?
                .first()
                .map(|hit| hit.0.id.as_str()),
            Some("legacy-row"),
            "non-leader reads through the current content index without waiting"
        );

        leader.release_fts_lease("migration-leader", token)?;
        assert!(reader.migrate_search_text_if_needed()?);
        assert_eq!(
            reader.get_meta("fts_state")?.as_deref(),
            Some("search_text")
        );
        assert_eq!(
            reader
                .search_working_fts("legacy", "/project")?
                .first()
                .map(|hit| hit.0.id.as_str()),
            Some("legacy-row")
        );

        drop(reader);
        drop(leader);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
        Ok(())
    }

    #[test]
    fn interrupted_fts_phases_resume_from_durable_state() -> anyhow::Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pi-memory-fts-resume-{nonce}.db"));
        let path = path.to_string_lossy().into_owned();
        let storage = TursoStorage::open_deferred(&path)?;
        storage.insert_working(&WorkingMemoryRecord {
            id: "resume-row".into(),
            content: "resume content after interrupted migration".into(),
            embed_text: None,
            source: None,
            session_id: "session".into(),
            project_scope: "/project".into(),
            importance: 0.5,
            memory_type: "working".into(),
            metadata: None,
            created_at: "2026-08-21T00:00:00Z".into(),
            updated_at: "2026-08-21T00:00:00Z".into(),
            consolidated_at: None,
            valid_until: None,
            superseded_by: None,
        })?;
        storage
            .conn
            .execute("UPDATE working_memory SET search_text = NULL WHERE id = 'resume-row'")?;
        storage.set_meta("fts_phase", "backfilling")?;
        storage.set_meta("fts_dirty", "true")?;
        drop(storage);

        let resumed = TursoStorage::open_deferred(&path)?;
        assert_eq!(resumed.get_meta("fts_state")?.as_deref(), Some("content"));
        assert_eq!(
            resumed
                .search_working_fts("resume", "/project")?
                .first()
                .map(|hit| hit.0.id.as_str()),
            Some("resume-row"),
            "partial backfill must leave the fallback index usable"
        );
        assert!(resumed.migrate_search_text_if_needed()?);
        assert_eq!(
            resumed.get_meta("fts_state")?.as_deref(),
            Some("search_text")
        );

        // Simulate process death after v2 index creation but before the state switch.
        resumed.set_meta("fts_state", "content")?;
        resumed.set_meta("fts_version", "1")?;
        resumed.set_meta("fts_phase", "index_ready")?;
        resumed.set_meta("fts_dirty", "true")?;
        drop(resumed);

        let recovered = TursoStorage::open_deferred(&path)?;
        assert_eq!(recovered.get_meta("fts_state")?.as_deref(), Some("content"));
        assert!(recovered.migrate_search_text_if_needed()?);
        assert_eq!(
            recovered.get_meta("fts_state")?.as_deref(),
            Some("search_text")
        );
        assert_eq!(
            recovered
                .search_working_fts("resume", "/project")?
                .first()
                .map(|hit| hit.0.id.as_str()),
            Some("resume-row")
        );

        drop(recovered);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
        Ok(())
    }

    #[test]
    fn fts_migration_worker_process() -> anyhow::Result<()> {
        let Ok(path) = std::env::var("PI_MEMORY_TEST_FTS_DB") else {
            return Ok(());
        };
        let storage = TursoStorage::open_deferred(&path)?;
        assert!(storage.migrate_search_text_if_needed()?);
        Ok(())
    }

    #[test]
    fn fts_migration_heartbeat_survives_default_production_lease() -> anyhow::Result<()> {
        const LEASE_MS: u64 = 30_000;
        const ROW_DELAY_MS: u64 = 65;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let base = std::env::temp_dir().join(format!("pi-memory-fts-production-lease-{nonce}"));
        std::fs::create_dir_all(&base)?;
        let path = base.join("memory.db").to_string_lossy().into_owned();
        let ready_path = base.join("worker.ready");
        let release_path = base.join("worker.release");
        let storage = TursoStorage::open_once_with_multiprocess_wal(&path, true)?;
        storage.begin_tx()?;
        let values = (0..701)
            .map(|index| {
                format!(
                    "('slow-legacy-{index:04}', 'production lease heartbeat searchable row {index:04}', 'session', '/project', 0.5, 'working', '2026-08-21T00:00:00Z', '2026-08-21T00:00:00Z', NULL)"
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        storage.conn.execute(format!(
            "INSERT INTO working_memory (
                 id, content, session_id, project_scope, importance, memory_type,
                 created_at, updated_at, search_text
             ) VALUES {values}"
        ))?;
        storage.commit_tx()?;
        drop(storage);
        // Open the observer before migration starts. Re-running schema initialization while
        // another connection is backfilling can invalidate its prepared statements.
        let reader = TursoStorage::open_once_with_multiprocess_wal(&path, true)?;

        let executable = std::env::current_exe()?;
        let migration_started = std::time::Instant::now();
        let worker = Command::new(&executable)
            .arg("--exact")
            .arg("storage::turso::fts_lease_tests::fts_migration_worker_process")
            .arg("--nocapture")
            .env("PI_MEMORY_TEST_FTS_DB", &path)
            .env("PI_MEMORY_TEST_FTS_READY_FILE", &ready_path)
            .env("PI_MEMORY_TEST_FTS_RELEASE_FILE", &release_path)
            .env("PI_MEMORY_TEST_FTS_PAUSE_PHASE", "backfill_batch")
            .env("PI_MEMORY_TEST_FTS_PAUSE_AFTER_BATCH", "5")
            .env("PI_MEMORY_TEST_FTS_ROW_DELAY_MS", ROW_DELAY_MS.to_string())
            .env("PI_MEMORY_FTS_MIGRATION_LEASE_MS", LEASE_MS.to_string())
            .env("PI_MEMORY_MULTIPROCESS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut worker = ChildGuard(worker);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !ready_path.exists() {
            if let Some(status) = worker.0.try_wait()? {
                anyhow::bail!(
                    "migration worker exited before the production-lease pause: {status}"
                );
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "migration worker did not reach the production-lease pause"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        anyhow::ensure!(
            migration_started.elapsed() >= std::time::Duration::from_millis(LEASE_MS),
            "worker must remain active past the original production TTL"
        );

        assert_eq!(reader.get_meta("fts_state")?.as_deref(), Some("content"));
        let lease_rows = reader
            .conn
            .prepare("SELECT expires_at FROM fts_migration_lock WHERE id = 1")?
            .run_collect_rows()?;
        let expiry =
            chrono::DateTime::parse_from_rfc3339(&super::value_to_string(&lease_rows[0][0]))?;
        let remaining_lease = expiry.with_timezone(&chrono::Utc) - chrono::Utc::now();
        assert!(
            remaining_lease > chrono::Duration::seconds(5),
            "heartbeat must keep the production lease live after its original 30-second TTL; remaining={remaining_lease:?}, elapsed={:?}",
            migration_started.elapsed()
        );
        assert!(
            !reader.migrate_search_text_if_needed()?,
            "a second process cannot take over a renewed production-duration lease"
        );

        // Avoid keeping a stale prepared schema handle alive while the worker creates v2 indexes.
        drop(reader);
        std::fs::write(&release_path, "continue")?;
        let status = worker.0.wait()?;
        anyhow::ensure!(status.success(), "migration worker failed: {status}");
        let completed = TursoStorage::open_once_with_multiprocess_wal(&path, true)?;
        assert_eq!(completed.count_null_search_text()?, 0);
        assert_eq!(
            completed.get_meta("fts_state")?.as_deref(),
            Some("search_text")
        );

        drop(completed);
        drop(worker);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }

    #[test]
    fn fts_migration_crash_recovers_across_processes() -> anyhow::Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let base = std::env::temp_dir().join(format!("pi-memory-fts-process-{nonce}"));
        std::fs::create_dir_all(&base)?;
        let path = base.join("memory.db").to_string_lossy().into_owned();
        let ready_path = base.join("worker.ready");
        let release_path = base.join("worker.release");

        let storage = TursoStorage::open_once_with_multiprocess_wal(&path, true)?;
        storage.begin_tx()?;
        for index in 0..501 {
            storage.insert_working(&WorkingMemoryRecord {
                id: format!("legacy-{index:04}"),
                content: format!("legacy migration searchable token {index:04}"),
                embed_text: None,
                source: None,
                session_id: "session".into(),
                project_scope: "/project".into(),
                importance: 0.5,
                memory_type: "working".into(),
                metadata: None,
                created_at: format!("2026-08-21T00:{:02}:00Z", index / 60),
                updated_at: "2026-08-21T00:00:00Z".into(),
                consolidated_at: None,
                valid_until: None,
                superseded_by: None,
            })?;
        }
        storage
            .conn
            .execute("UPDATE working_memory SET search_text = NULL")?;
        storage.commit_tx()?;
        drop(storage);

        let executable = std::env::current_exe()?;
        let migration_started = std::time::Instant::now();
        let mut worker = Command::new(&executable)
            .arg("--exact")
            .arg("storage::turso::fts_lease_tests::fts_migration_worker_process")
            .arg("--nocapture")
            .env("PI_MEMORY_TEST_FTS_DB", &path)
            .env("PI_MEMORY_TEST_FTS_READY_FILE", &ready_path)
            .env("PI_MEMORY_TEST_FTS_RELEASE_FILE", &release_path)
            .env("PI_MEMORY_TEST_FTS_PAUSE_PHASE", "backfill_batch")
            .env("PI_MEMORY_TEST_FTS_ROW_DELAY_MS", "35")
            .env("PI_MEMORY_FTS_MIGRATION_LEASE_MS", "3000")
            .env("PI_MEMORY_MULTIPROCESS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready_path.exists() {
            if let Some(status) = worker.try_wait()? {
                anyhow::bail!("migration worker exited before its first-batch pause: {status}");
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "migration worker did not reach the first-batch pause"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            migration_started.elapsed() >= std::time::Duration::from_secs(3),
            "worker must remain active longer than its original lease TTL"
        );

        let reader = TursoStorage::open_once_with_multiprocess_wal(&path, true)?;
        assert_eq!(reader.get_meta("fts_state")?.as_deref(), Some("content"));
        assert_eq!(reader.count_null_search_text()?, 401);
        let lease_expiry_rows = reader
            .conn
            .prepare("SELECT expires_at FROM fts_migration_lock WHERE id = 1")?
            .run_collect_rows()?;
        let lease_expiry = super::value_to_string(&lease_expiry_rows[0][0]);
        let lease_expiry = chrono::DateTime::parse_from_rfc3339(&lease_expiry)?;
        assert!(
            lease_expiry > chrono::Utc::now() + chrono::Duration::seconds(1),
            "the long-running worker must renew its lease before the first batch commits"
        );
        assert!(
            !reader.migrate_search_text_if_needed()?,
            "a second process cannot become leader while the worker lease is live"
        );
        assert_eq!(
            reader
                .search_working_fts_top_k("legacy", "/project", 20)?
                .len(),
            20,
            "readers continue using the old index while another process backfills"
        );

        worker.kill()?;
        let _ = worker.wait()?;
        std::thread::sleep(std::time::Duration::from_millis(3_100));
        assert!(reader.migrate_search_text_if_needed()?);
        assert_eq!(reader.count_null_search_text()?, 0);
        assert_eq!(
            reader.get_meta("fts_state")?.as_deref(),
            Some("search_text")
        );
        assert_eq!(
            reader
                .search_working_fts_top_k("legacy", "/project", 20)?
                .len(),
            20
        );

        // Kill a second leader after v2 indexes are ready but before it can switch readers.
        reader.set_meta("fts_state", "content")?;
        reader.set_meta("fts_version", "1")?;
        reader.set_meta("fts_phase", "backfilled")?;
        reader.set_meta("fts_dirty", "true")?;
        let switch_ready_path = base.join("switch-worker.ready");
        let switch_release_path = base.join("switch-worker.release");
        let mut switch_worker = Command::new(&executable)
            .arg("--exact")
            .arg("storage::turso::fts_lease_tests::fts_migration_worker_process")
            .arg("--nocapture")
            .env("PI_MEMORY_TEST_FTS_DB", &path)
            .env("PI_MEMORY_TEST_FTS_READY_FILE", &switch_ready_path)
            .env("PI_MEMORY_TEST_FTS_RELEASE_FILE", &switch_release_path)
            .env(
                "PI_MEMORY_TEST_FTS_PAUSE_PHASE",
                "index_ready_before_switch",
            )
            .env("PI_MEMORY_FTS_MIGRATION_LEASE_MS", "3000")
            .env("PI_MEMORY_MULTIPROCESS", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !switch_ready_path.exists() {
            if let Some(status) = switch_worker.try_wait()? {
                anyhow::bail!("switch worker exited before its pause: {status}");
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "switch worker did not reach the pre-switch pause"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(reader.get_meta("fts_state")?.as_deref(), Some("content"));
        assert_eq!(reader.get_meta("fts_phase")?.as_deref(), Some("backfilled"));
        assert!(reader.fts_index_exists("fts_working_idx_v2")?);
        assert_eq!(
            reader
                .search_working_fts_top_k("legacy", "/project", 20)?
                .len(),
            20,
            "readers stay on content until the indexed state switch commits"
        );
        switch_worker.kill()?;
        let _ = switch_worker.wait()?;
        std::thread::sleep(std::time::Duration::from_millis(3_100));
        assert!(reader.migrate_search_text_if_needed()?);
        assert_eq!(
            reader.get_meta("fts_state")?.as_deref(),
            Some("search_text")
        );
        assert_eq!(
            reader.get_meta("fts_phase")?.as_deref(),
            Some("search_text")
        );

        drop(reader);
        let _ = std::fs::remove_dir_all(&base);
        Ok(())
    }
}
