//! 存储层：基于 turso_core（SQLite 兼容）的持久化。
//!
//! 设计要点：
//! - `Storage` trait 隔离引擎实现（当前 turso_core，保留将来切回 rusqlite 的可行性）；
//! - 表结构迁移自 TS 版 schema.ts，保持字段与语义一致；
//! - FTS 使用 Turso Tantivy 语法（M1 验证具体语法后落地，见 T2）。

pub mod mem;

/// 存储引擎抽象：隔离 turso_core 的具体 API，方便测试与未来替换。
pub trait Storage: Send + Sync {
    /// 初始化 schema（幂等）。
    fn init(&mut self) -> anyhow::Result<()>;
    /// 健康自检：返回数据库路径等诊断信息。
    fn diagnose(&self) -> anyhow::Result<serde_json::Value>;
}
