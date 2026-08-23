//! turso_core file storage implementation (M1-T2).
//!
//! Table schemas are ported from the TS `schema.ts`; CRUD semantics match the TS `store.ts`
//! (upsert via ON CONFLICT DO UPDATE). FTS uses Turso Tantivy syntax:
//! `CREATE INDEX ... USING fts` attaches to existing tables and is maintained automatically (inserts are covered by tests;
//! the update sync behavior is covered by `tests/turso_storage.rs::turso_fts_update_sync`).

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use turso_core::types::{Text, Value};
use turso_core::{Connection, Database, DatabaseOpts, Numeric, OpenFlags, Row, Statement};

use crate::models::{
    EpisodicMemoryRecord, FactRecord, InstructionRecord, MemoryEmbeddingRecord, MemoryKind,
    PreferenceRecord, SessionStateRecord, TimelineEventRecord, WorkingMemoryRecord,
};

use super::Storage;

/// Table schema SQL (kept aligned with the TS schema.ts).
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS working_memory (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
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
    valid_until TEXT,
    superseded_by TEXT
);
CREATE TABLE IF NOT EXISTS episodic_memory (
    id TEXT PRIMARY KEY,
    content TEXT NOT NULL,
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
    PRIMARY KEY (memory_kind, memory_id)
);
CREATE TABLE IF NOT EXISTS session_state (
    session_id TEXT PRIMARY KEY,
    project_scope TEXT NOT NULL,
    last_retained_user_turn INTEGER NOT NULL DEFAULT 0,
    has_recalled_for_first_turn INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT DEFAULT CURRENT_TIMESTAMP
);
"#;

/// turso_core file storage.
pub struct TursoStorage {
    conn: Arc<Connection>,
    path: String,
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

    fn open_once(path: &str) -> anyhow::Result<Self> {        let io = Database::io_for_path(path)?;
        // The Tantivy FTS index (USING fts) is an experimental index method and must be enabled explicitly.
        // libSQL's experimental multiprocess WAL (which lifts the exclusive file lock so multiple pi
        // sessions can share one project DB) is OFF by default: it can panic on large transactions
        // ("shared WAL frame ids must increase monotonically"). Enable explicitly with
        // PI_MEMORY_MULTIPROCESS=1 and accept the experimental risk; busy_timeout applies in both modes.
        let enable_multiprocess_wal = std::env::var("PI_MEMORY_MULTIPROCESS").is_ok();
        let opts = DatabaseOpts::new()
            .with_index_method(true)
            .with_multiprocess_wal(enable_multiprocess_wal);
        let db = Database::open_file_with_flags(io, path, OpenFlags::default(), opts, None)?;
        let conn = db.connect()?;
        let storage = Self {
            conn,
            path: path.to_string(),
            stmts: Mutex::new(std::collections::HashMap::new()),
        };
        storage.init()?;
        // Multi-session resilience: a busy_timeout turns cross-process write contention into a bounded
        // wait instead of an immediate error, so concurrent pi sessions rarely fail on retain
        storage.conn.execute("PRAGMA busy_timeout = 3000")?;
        Ok(storage)
    }

    /// Initialize the schema (idempotent).
    fn init(&self) -> anyhow::Result<()> {
        self.conn.prepare_execute_batch(SCHEMA_SQL)?;
        // Legacy migration: add project_scope to structured tables (new DBs already have it; idempotent)
        self.ensure_structured_scope_columns()?;
        // Tantivy FTS index attaches to existing tables (Turso syntax, not an SQLite FTS5 virtual table)
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_working_idx ON working_memory USING fts (content)",
        )?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_episodes_idx ON episodic_memory USING fts (content)",
        )?;
        // 结构化表 scope 索引：LIKE %q% 无法走索引，但先按 project_scope 裁剪可避免跨项目全表扫描
        for table in ["facts", "preferences", "instructions", "timeline"] {
            let sql = format!("CREATE INDEX IF NOT EXISTS idx_{table}_scope ON {table}(project_scope)");
            self.conn.prepare_execute_batch(&sql)?;
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
                let alter = format!("ALTER TABLE {table} ADD COLUMN project_scope TEXT NOT NULL DEFAULT ''");
                self.conn.prepare_execute_batch(&alter)?;
            }
        }
        Ok(())
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
                consolidated_at, valid_until, superseded_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
                superseded_by = excluded.superseded_by",
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

    /// Begin a transaction (bulk-write performance: the Tantivy FTS index updates once at COMMIT).
    pub fn begin_tx(&self) -> anyhow::Result<()> {
        self.conn.execute("BEGIN")?;
        Ok(())
    }

    /// Commit the transaction.
    pub fn commit_tx(&self) -> anyhow::Result<()> {
        self.conn.execute("COMMIT")?;
        Ok(())
    }

    /// Roll back the current transaction (used when a bulk operation fails midway).
    pub fn rollback_tx(&self) -> anyhow::Result<()> {
        self.conn.execute("ROLLBACK")?;
        Ok(())
    }

    /// Migrate data from a legacy pi-memory SQLite database (M3 migration tool).
    /// The legacy DB is read directly by turso_core (SQLite file compatible); FTS5 virtual tables are ignored (the new Tantivy index rebuilds automatically).
    /// Performance: drop the FTS index before migrating (per-row commits degrade nonlinearly), then rebuild once at the end.
    pub fn migrate_from(&self, source_path: &str) -> anyhow::Result<serde_json::Value> {
        let source = TursoStorage::open(source_path)?;
        let mut stats = serde_json::Map::new();

        // Bulk import phase: drop the FTS index first (per-row Tantivy commits degrade nonlinearly with size)
        self.drop_fts()?;
        self.begin_tx()?;

        let result = (|| -> anyhow::Result<()> {
            // working_memory
            let rows = source.list_all_working()?;
            for row in rows {
                self.insert_working(&row)?;
            }
            stats.insert("working".into(), serde_json::json!(self.count_working()?));

            // episodic_memory
            let rows = source.list_all_episodic()?;
            for row in rows {
                self.insert_episodic(&row)?;
            }
            stats.insert("episodic".into(), serde_json::json!(self.count_episodic()?));

            // facts / preferences / instructions / timeline
            let facts = source.list_all_facts()?;
            self.insert_facts(&facts)?;
            stats.insert("facts".into(), serde_json::json!(facts.len()));
            let preferences = source.list_all_preferences()?;
            self.insert_preferences(&preferences)?;
            stats.insert("preferences".into(), serde_json::json!(preferences.len()));
            let instructions = source.list_all_instructions()?;
            self.insert_instructions(&instructions)?;
            stats.insert("instructions".into(), serde_json::json!(instructions.len()));
            let timeline = source.list_all_timeline()?;
            self.insert_timeline_events(&timeline)?;
            stats.insert("timeline".into(), serde_json::json!(timeline.len()));

            // embeddings (vectors migrate directly; model metadata preserved)
            let embeddings = source.list_all_embeddings()?;
            let embedding_count = embeddings.len();
            for embedding in &embeddings {
                self.save_embedding(embedding)?;
            }
            stats.insert("embeddings".into(), serde_json::json!(embedding_count));
            Ok(())
        })();

        if let Err(e) = result {
            // Restore consistency on failure: roll back the partial import and rebuild the FTS index
            let _ = self.rollback_tx();
            let _ = self.rebuild_fts();
            return Err(e);
        }
        self.commit_tx()?;
        // Rebuild the FTS index (one full build after migration)
        self.rebuild_fts()?;

        Ok(serde_json::Value::Object(stats))
    }

    /// List all working memories (for migration).
    pub fn list_all_working(&self) -> anyhow::Result<Vec<WorkingMemoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, embed_text, source, session_id, project_scope,\n                    importance, memory_type, metadata_json, created_at, updated_at,\n                    consolidated_at, valid_until, superseded_by\n             FROM working_memory",
        )?;
        collect_rows(&mut stmt, working_from_row)
    }

    pub fn list_all_episodic(&self) -> anyhow::Result<Vec<EpisodicMemoryRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, summary_of, session_id, project_scope,\n                    importance, tier, metadata_json, memory_type,\n                    created_at, updated_at, degraded_at, valid_until, superseded_by\n             FROM episodic_memory",
        )?;
        collect_rows(&mut stmt, episodic_from_row)
    }

    pub fn list_all_facts(&self) -> anyhow::Result<Vec<FactRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT fact_id, subject, predicate, object, confidence, source_memory_id, session_id, project_scope, timestamp\n             FROM facts",
        )?;
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
        let mut stmt = self.conn.prepare(
            "SELECT id, preference, topic, context_snippet, source_memory_id, session_id, project_scope, created_at\n             FROM preferences",
        )?;
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
        let mut stmt = self.conn.prepare(
            "SELECT id, instruction, topic, active, context_snippet, source_memory_id, session_id, project_scope, created_at\n             FROM instructions",
        )?;
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
        let mut stmt = self.conn.prepare(
            "SELECT event_id, date, description, source, source_memory_id, session_id, project_scope, created_at\n             FROM timeline",
        )?;
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
        let mut stmt = self.conn.prepare(
            "SELECT memory_kind, memory_id, embedding_json, model\n             FROM memory_embeddings",
        )?;
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
                embedding: serde_json::from_str(&row_to_string(row, 2)?).unwrap_or_default(),
                model: row_to_string(row, 3)?,
            })
        })
    }

    /// List working memories for a project (created_at DESC).
    pub fn list_working_by_project(&self, project_scope: &str) -> anyhow::Result<Vec<WorkingMemoryRecord>> {
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
        let mut cache = self.stmts.lock().expect("mutex poisoned");
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
        self.with_cached_stmt(
            "SELECT id, content, embed_text, source, session_id, project_scope,
                    importance, memory_type, metadata_json, created_at, updated_at,
                    consolidated_at, valid_until, superseded_by,
                    fts_score(content, ?1) AS score
             FROM working_memory
             WHERE fts_match(content, ?1) AND project_scope = ?2
             ORDER BY score DESC",
            |stmt| {
                bind_text(stmt, 1, query)?;
                bind_text(stmt, 2, project_scope)?;
                collect_rows_with_score(stmt, working_from_row)
            },
        )
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
                created_at, updated_at, degraded_at, valid_until, superseded_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
                superseded_by = excluded.superseded_by",
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
        stmt.run_ignore_rows()?;
        Ok(())
    }

    /// List episodic memories for a project (created_at DESC).
    pub fn list_episodic_by_project(&self, project_scope: &str) -> anyhow::Result<Vec<EpisodicMemoryRecord>> {
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
        self.with_cached_stmt(
            "SELECT id, content, summary_of, session_id, project_scope,
                    importance, tier, metadata_json, memory_type,
                    created_at, updated_at, degraded_at, valid_until, superseded_by,
                    fts_score(content, ?1) AS score
             FROM episodic_memory
             WHERE fts_match(content, ?1) AND project_scope = ?2
             ORDER BY score DESC",
            |stmt| {
                bind_text(stmt, 1, query)?;
                bind_text(stmt, 2, project_scope)?;
                collect_rows_with_score(stmt, episodic_from_row)
            },
        )
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

    /// Count working/episodic rows per project (used by the stats command).
    pub fn count_working_by_project(&self, project_scope: &str) -> anyhow::Result<i64> {
        count_rows_where(&self.conn, "working_memory", "project_scope", project_scope)
    }

    pub fn count_episodic_by_project(&self, project_scope: &str) -> anyhow::Result<i64> {
        count_rows_where(&self.conn, "episodic_memory", "project_scope", project_scope)
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
    pub fn search_facts_like(&self, query: &str, project_scope: &str) -> anyhow::Result<Vec<FactRecord>> {
        self.with_cached_stmt(
            "SELECT fact_id, subject, predicate, object, confidence, source_memory_id, session_id, project_scope, timestamp
             FROM facts WHERE (subject LIKE ?1 OR predicate LIKE ?1 OR object LIKE ?1)
               AND (project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            |stmt| {
                bind_text(stmt, 1, &format!("%{query}%"))?;
                bind_text(stmt, 2, project_scope)?;
                collect_rows(stmt, |row| {
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
            },
        )
    }

    pub fn search_instructions_like(&self, query: &str, project_scope: &str) -> anyhow::Result<Vec<InstructionRecord>> {
        self.with_cached_stmt(
            "SELECT id, instruction, topic, active, context_snippet, source_memory_id, session_id, project_scope, created_at
             FROM instructions WHERE instruction LIKE ?1
               AND (project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            |stmt| {
                bind_text(stmt, 1, &format!("%{query}%"))?;
                bind_text(stmt, 2, project_scope)?;
                collect_rows(stmt, |row| {
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
            },
        )
    }

    pub fn search_preferences_like(&self, query: &str, project_scope: &str) -> anyhow::Result<Vec<PreferenceRecord>> {
        self.with_cached_stmt(
            "SELECT id, preference, topic, context_snippet, source_memory_id, session_id, project_scope, created_at
             FROM preferences WHERE preference LIKE ?1
               AND (project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            |stmt| {
                bind_text(stmt, 1, &format!("%{query}%"))?;
                bind_text(stmt, 2, project_scope)?;
                collect_rows(stmt, |row| {
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
            },
        )
    }

    pub fn search_timeline_like(&self, query: &str, project_scope: &str) -> anyhow::Result<Vec<TimelineEventRecord>> {
        self.with_cached_stmt(
            "SELECT event_id, date, description, source, source_memory_id, session_id, project_scope, created_at
             FROM timeline WHERE (date LIKE ?1 OR description LIKE ?1)
               AND (project_scope = ?2 OR project_scope = '' OR project_scope IS NULL)",
            |stmt| {
                bind_text(stmt, 1, &format!("%{query}%"))?;
                bind_text(stmt, 2, project_scope)?;
                collect_rows(stmt, |row| {
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
            },
        )
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
        let mut stmt = self.conn.prepare(
            "INSERT INTO memory_embeddings (memory_kind, memory_id, embedding_json, model)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(memory_kind, memory_id) DO UPDATE SET
                embedding_json = excluded.embedding_json,
                model = excluded.model,
                created_at = CURRENT_TIMESTAMP",
        )?;
        bind_text(&mut stmt, 1, record.memory_kind.as_str())?;
        bind_text(&mut stmt, 2, &record.memory_id)?;
        bind_text(&mut stmt, 3, &serde_json::to_string(&record.embedding)?)?;
        bind_text(&mut stmt, 4, &record.model)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    pub fn get_embedding(&self, memory_kind: MemoryKind, memory_id: &str) -> anyhow::Result<Option<MemoryEmbeddingRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT memory_kind, memory_id, embedding_json, model
             FROM memory_embeddings WHERE memory_kind = ?1 AND memory_id = ?2",
        )?;
        bind_text(&mut stmt, 1, memory_kind.as_str())?;
        bind_text(&mut stmt, 2, memory_id)?;
        let mut found = None;
        let mut err = None;
        stmt.run_with_row_callback(|row| {
            match (
                row_to_string(row, 0),
                row_to_string(row, 1),
                row_to_string(row, 2),
                row_to_string(row, 3),
            ) {
                (Ok(kind), Ok(id), Ok(embedding_json), Ok(model)) => {
                    let embedding = serde_json::from_str(&embedding_json).unwrap_or_default();
                    let memory_kind = match kind.as_str() {
                        "working" => MemoryKind::Working,
                        "episodic" => MemoryKind::Episodic,
                        _ => return Ok(()),
                    };
                    found = Some(MemoryEmbeddingRecord { memory_kind, memory_id: id, embedding, model });
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
    }

    // -----------------------------------------------------------------------
    // session_state
    // -----------------------------------------------------------------------

    pub fn get_session_state(&self, session_id: &str) -> anyhow::Result<Option<SessionStateRecord>> {
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
    pub fn mark_working_consolidated(&self, ids: &[String], consolidated_at: &str) -> anyhow::Result<()> {
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

    /// Delete working/episodic memories for a project (used by the clear command).
    pub fn clear_project_memories(&self, project_scope: &str) -> anyhow::Result<()> {
        for table in ["working_memory", "episodic_memory"] {
            let sql = format!("DELETE FROM {table} WHERE project_scope = ?1");
            let mut stmt = self.conn.prepare(&sql)?;
            bind_text(&mut stmt, 1, project_scope)?;
            stmt.run_ignore_rows()?;
        }
        // Clear global tables (matches the TS clear)
        for table in [
            "memory_embeddings", "facts", "preferences", "instructions", "timeline", "session_state",
        ] {
            let sql = format!("DELETE FROM {table}");
            self.conn.execute(&sql)?;
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // FTS rebuild (/memory rebuild semantics)
    // -----------------------------------------------------------------------

    /// Drop the FTS index (avoids per-row commits during bulk fill; rebuild_fts rebuilds it once afterwards).
    pub fn drop_fts(&self) -> anyhow::Result<()> {
        self.conn.execute("DROP INDEX IF EXISTS fts_working_idx")?;
        self.conn.execute("DROP INDEX IF EXISTS fts_episodes_idx")?;
        Ok(())
    }

    /// Rebuild the Tantivy FTS index: DROP + CREATE (idempotent).
    pub fn rebuild_fts(&self) -> anyhow::Result<()> {
        self.conn.execute("DROP INDEX IF EXISTS fts_working_idx")?;
        self.conn.execute("DROP INDEX IF EXISTS fts_episodes_idx")?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_working_idx ON working_memory USING fts (content)",
        )?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_episodes_idx ON episodic_memory USING fts (content)",
        )?;
        Ok(())
    }
}

impl Storage for TursoStorage {
    fn init(&mut self) -> anyhow::Result<()> {
        TursoStorage::init(self)
    }

    fn diagnose(&self) -> anyhow::Result<serde_json::Value> {
        let working = self.count_working()?;
        let episodic = self.count_episodic()?;
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
                if texts.is_empty() || texts.iter().all(|t| t.is_empty()) || texts.iter().any(|t| t == "ok") {
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
            "episodic_memory_count": episodic,
            "fts": "tantivy",
            "integrity": integrity_status,
        }))
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
fn collect_rows<T>(stmt: &mut Statement, parse: fn(&Row) -> anyhow::Result<T>) -> anyhow::Result<Vec<T>> {
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

fn count_rows_where(conn: &Arc<Connection>, table: &str, column: &str, value: &str) -> anyhow::Result<i64> {
    let sql = format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1");
    let mut stmt = conn.prepare(&sql)?;
    bind_text(&mut stmt, 1, value)?;
    let rows = stmt.run_collect_rows()?;
    Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Value binding and extraction helpers (thin wrappers over the turso_core low-level API)
// ---------------------------------------------------------------------------

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

