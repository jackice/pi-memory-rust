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
pub mod protocol;
pub mod runtime;
pub mod storage;
pub mod vector_index;
