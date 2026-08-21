//! turso_core 文件存储实现（M1-T2 起步）。
//!
//! 表结构移植自 TS 版 `schema.ts`（字段与语义一致）；FTS 部分使用
//! Turso Tantivy 语法（语法验证后落地，见 `init` 注释）。

use std::num::NonZeroUsize;
use std::sync::Arc;

use turso_core::types::{Text, Value};
use turso_core::{Connection, Database, DatabaseOpts, Numeric, OpenFlags, Row, Statement};

use super::Storage;

/// 表结构 SQL（与 TS 版 schema.ts 保持一致）。
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
    timestamp TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS preferences (
    id TEXT PRIMARY KEY,
    preference TEXT NOT NULL,
    topic TEXT,
    context_snippet TEXT,
    source_memory_id TEXT,
    session_id TEXT NOT NULL,
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
    created_at TEXT DEFAULT CURRENT_TIMESTAMP
);
CREATE TABLE IF NOT EXISTS timeline (
    event_id TEXT PRIMARY KEY,
    date TEXT NOT NULL,
    description TEXT NOT NULL,
    source TEXT,
    source_memory_id TEXT,
    session_id TEXT NOT NULL,
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

/// FTS 说明：Tantivy 索引挂在现有表上（`CREATE INDEX ... USING fts`），
/// 与 TS 版 SQLite FTS5 虚拟表语义对应（仅索引 content 列）。
const _FTS_NOTES: &str = "";

/// working_memory 行（M1 起步子集，字段随移植进度扩展）。
#[derive(Debug, Clone)]
pub struct WorkingMemoryRow {
    pub id: String,
    pub content: String,
    pub session_id: String,
    pub project_scope: String,
    pub importance: f64,
    pub memory_type: String,
    pub created_at: String,
}

/// turso_core 文件存储。
pub struct TursoStorage {
    conn: Arc<Connection>,
    path: String,
}

impl TursoStorage {
    /// 打开（或创建）本地数据库文件并初始化 schema。
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let io = Database::io_for_path(path)?;
        // Tantivy FTS 索引（USING fts）属 experimental index method，需显式开启
        let opts = DatabaseOpts::new().with_index_method(true);
        let db = Database::open_file_with_flags(io, path, OpenFlags::default(), opts, None)?;
        let conn = db.connect()?;
        let storage = Self { conn, path: path.to_string() };
        storage.init()?;
        Ok(storage)
    }

    /// 初始化 schema（幂等）。
    fn init(&self) -> anyhow::Result<()> {
        self.conn.prepare_execute_batch(SCHEMA_SQL)?;
        // Tantivy FTS 索引挂在现有表上（Turso 语法，非 SQLite FTS5 虚拟表）
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_working_idx ON working_memory USING fts (content)",
        )?;
        self.conn.prepare_execute_batch(
            "CREATE INDEX IF NOT EXISTS fts_episodes_idx ON episodic_memory USING fts (content)",
        )?;
        Ok(())
    }

    /// 插入一条 working memory。
    pub fn insert_working(&self, row: &WorkingMemoryRow) -> anyhow::Result<()> {
        let mut stmt = self.conn.prepare(
            "INSERT INTO working_memory (id, content, session_id, project_scope, importance, memory_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        bind_text(&mut stmt, 1, &row.id)?;
        bind_text(&mut stmt, 2, &row.content)?;
        bind_text(&mut stmt, 3, &row.session_id)?;
        bind_text(&mut stmt, 4, &row.project_scope)?;
        bind_num(&mut stmt, 5, row.importance)?;
        bind_text(&mut stmt, 6, &row.memory_type)?;
        stmt.run_ignore_rows()?;
        Ok(())
    }

    /// 按 id 查询 working memory。
    pub fn get_working(&self, id: &str) -> anyhow::Result<Option<WorkingMemoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, session_id, project_scope, importance, memory_type, created_at
             FROM working_memory WHERE id = ?1",
        )?;
        bind_text(&mut stmt, 1, id)?;
        let mut found = None;
        // 回调返回 turso_core 的 Result，无法直接传播 anyhow::Error，先记录后统一处理
        let mut callback_error = None;
        stmt.run_with_row_callback(|row| {
            match row_to_working(row) {
                Ok(r) => {
                    found = Some(r);
                    Ok(())
                }
                Err(e) => {
                    callback_error = Some(e);
                    Ok(())
                }
            }
        })?;
        if let Some(e) = callback_error {
            return Err(e);
        }
        Ok(found)
    }

    /// 统计 working_memory 条数。
    pub fn count_working(&self) -> anyhow::Result<i64> {
        let rows = self.conn.prepare("SELECT COUNT(*) FROM working_memory")?.run_collect_rows()?;
        Ok(rows.first().map(|row| value_to_i64(&row[0])).unwrap_or(0))
    }

    /// Tantivy FTS 搜索（M1-T2 验证语法：`CREATE INDEX ... USING fts`）。
    /// 返回 (id, content, score)。
    pub fn search_working_fts(&self, query: &str) -> anyhow::Result<Vec<(String, String, f64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, content, fts_score(content, ?1) AS score
             FROM working_memory WHERE fts_match(content, ?1) ORDER BY score DESC",
        )?;
        bind_text(&mut stmt, 1, query)?;
        let mut results = Vec::new();
        let mut callback_error = None;
        stmt.run_with_row_callback(|row| {
            match (row_to_string(row, 0), row_to_string(row, 1), row_to_f64(row, 2)) {
                (Ok(id), Ok(content), Ok(score)) => {
                    results.push((id, content, score));
                    Ok(())
                }
                (Err(e), _, _) => {
                    callback_error = Some(e);
                    Ok(())
                }
                _ => Ok(()),
            }
        })?;
        if let Some(e) = callback_error {
            return Err(e);
        }
        Ok(results)
    }
}

fn row_to_working(row: &Row) -> anyhow::Result<WorkingMemoryRow> {
    Ok(WorkingMemoryRow {
        id: row_to_string(row, 0)?,
        content: row_to_string(row, 1)?,
        session_id: row_to_string(row, 2)?,
        project_scope: row_to_string(row, 3)?,
        importance: row_to_f64(row, 4)?,
        memory_type: row_to_string(row, 5)?,
        created_at: row_to_string(row, 6)?,
    })
}

// ---------------------------------------------------------------------------
// Value 绑定与取值辅助（turso_core 底层 API 的轻量封装）
// ---------------------------------------------------------------------------

fn bind_text(stmt: &mut Statement, index: usize, value: &str) -> anyhow::Result<()> {
    stmt.bind_at(
        NonZeroUsize::new(index).expect("索引必须非零"),
        Value::Text(Text::new(value.to_string())),
    )?;
    Ok(())
}

fn bind_num(stmt: &mut Statement, index: usize, value: f64) -> anyhow::Result<()> {
    // turso_core 的 Numeric 无 From<f64>，经 NonNan 构造；NaN 按 NULL 处理
    let numeric = match turso_core::NonNan::new(value) {
        Some(non_nan) => Numeric::Float(non_nan),
        None => return Ok(()), // NaN 绑定为 NULL
    };
    stmt.bind_at(
        NonZeroUsize::new(index).expect("索引必须非零"),
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

fn row_to_f64(row: &Row, index: usize) -> anyhow::Result<f64> {
    let value = row.get_value(index);
    Ok(value_to_f64(value))
}

fn value_to_f64(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Numeric(n) => n.to_f64(),
        Value::Text(text) => text.as_str().parse().unwrap_or(0.0),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).parse().unwrap_or(0.0),
    }
}

fn value_to_i64(value: &Value) -> i64 {
    match value {
        Value::Numeric(Numeric::Integer(i)) => *i,
        Value::Numeric(Numeric::Float(f)) => Numeric::Float(*f).to_f64() as i64,
        _ => 0,
    }
}

impl Storage for TursoStorage {
    fn init(&mut self) -> anyhow::Result<()> {
        TursoStorage::init(self)
    }

    fn diagnose(&self) -> anyhow::Result<serde_json::Value> {
        let working = self.count_working()?;
        Ok(serde_json::json!({
            "backend": "turso",
            "path": self.path,
            "working_memory_count": working,
            "fts": "tantivy",
        }))
    }
}
