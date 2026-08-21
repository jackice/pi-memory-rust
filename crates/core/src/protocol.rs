//! JSON-RPC 2.0 协议类型（TS 客户端 ↔ Rust 常驻进程）。
//!
//! 设计约束：
//! - 请求/响应均为小负载（prompt、Top-K 候选、配置），重活（embedding 批量、索引检索）在子进程内完成；
//! - 帧格式：单行 JSON（`\n` 分隔），带 length 前缀防粘包：`<u32 长度字节><JSON 字节>`；
//! - 版本握手：`hello` 请求交换协议版本，版本不匹配立即报错退出。

use serde::{Deserialize, Serialize};

/// 当前协议版本，随不兼容变更递增。
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcResponse {
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

// ---------------------------------------------------------------------------
// 方法与参数
// ---------------------------------------------------------------------------

/// 支持的 RPC 方法名（M1 起步集）。
pub mod methods {
    /// 版本握手。params: { "version": u32 }
    pub const HELLO: &str = "hello";
    /// 健康自检。params: {} → 返回诊断信息
    pub const DIAGNOSE: &str = "diagnose";
    /// 关闭服务。params: {} → 进程退出
    pub const SHUTDOWN: &str = "shutdown";
    /// 召回注入。params: { prompt, projectScope, sessionId }
    pub const BUILD_RECALL_INJECTION: &str = "buildRecallInjection";
    /// 节奏 retain。params: { turnIndex, text, sessionId, projectScope }
    pub const RETAIN_TURN_MEMORIES: &str = "retainTurnMemories";
    /// 会话整合。params: { sessionId, projectScope }
    pub const CONSOLIDATE_SESSION_MEMORIES: &str = "consolidateSessionMemories";
    /// /memory 命令。params: { sub, target, projectScope, sessionId }
    pub const MEMORY_COMMAND: &str = "memoryCommand";
}

/// hello 请求参数
#[derive(Debug, Serialize, Deserialize)]
pub struct HelloParams {
    pub version: u32,
}

/// hello 响应结果
#[derive(Debug, Serialize, Deserialize)]
pub struct HelloResult {
    pub version: u32,
    pub name: String,
}

/// 标准错误码
pub mod codes {
    /// 请求方法不存在
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// 参数无效
    pub const INVALID_PARAMS: i32 = -32602;
    /// 内部错误
    pub const INTERNAL_ERROR: i32 = -32603;
    /// 协议版本不匹配
    pub const VERSION_MISMATCH: i32 = -32000;
}

impl RpcResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Self { id, result: Some(result), error: None }
    }

    pub fn err(id: u64, code: i32, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}
