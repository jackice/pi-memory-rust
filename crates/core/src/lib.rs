//! pi-memory 核心库。
//!
//! 纯 Rust 实现：存储（turso_core）、规则提取/整合、混合召回。
//! 与 TS 参考实现（pi-memory v0.1.2）保持行为等价（bit-exact 契约）。

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

/// 信息级诊断日志：仅当设置 PI_MEMORY_DEBUG=1 时输出到 stderr。
/// 子进程 stderr 会被宿主透传，直接输出会污染 pi 的 TUI 渲染；
/// 错误信息已通过 RPC 错误响应返回，stderr 仅作额外诊断。
pub fn debug_log(msg: &str) {
    if std::env::var("PI_MEMORY_DEBUG").is_ok() {
        eprintln!("{msg}");
    }
}
