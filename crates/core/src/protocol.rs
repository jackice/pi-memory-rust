//! JSON-RPC 2.0 protocol types (TS client ↔ Rust resident process).
//!
//! Design constraints:
//! - requests/responses are small payloads (prompt, top-K candidates, config); heavy work (embedding batches, index search) runs inside the child process;
//! - frame format: single-line JSON separated by `\n`, with a length prefix to avoid sticky frames: `<u32 length bytes><JSON bytes>`;
//! - version handshake: the `hello` request exchanges the protocol version; a mismatch fails fast.

use serde::{Deserialize, Serialize};

/// Current protocol version; bumped on incompatible changes.
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
// Methods and params
// ---------------------------------------------------------------------------

/// Supported RPC method names (M1 initial set).
pub mod methods {
    /// Version handshake. params: { "version": u32 }
    pub const HELLO: &str = "hello";
    /// Health check. params: {} → returns diagnostics
    pub const DIAGNOSE: &str = "diagnose";
    /// Shut down the service. params: {} → process exits
    pub const SHUTDOWN: &str = "shutdown";
    /// Recall injection. params: { prompt, projectScope, sessionId }
    pub const BUILD_RECALL_INJECTION: &str = "buildRecallInjection";
    /// Rhythmic retain. params: { turnIndex, text, sessionId, projectScope }
    pub const RETAIN_TURN_MEMORIES: &str = "retainTurnMemories";
    /// Session consolidation. params: { sessionId, projectScope }
    pub const CONSOLIDATE_SESSION_MEMORIES: &str = "consolidateSessionMemories";
    /// /memory command. params: { sub, target, projectScope, sessionId }
    pub const MEMORY_COMMAND: &str = "memoryCommand";
    /// Manually trigger model download. params: {}
    pub const PREPARE_MODEL: &str = "prepareModel";
    /// Migrate a legacy SQLite database. params: { sourcePath }
    pub const MIGRATE: &str = "migrate";
    /// Fetch LLM prompt templates. params: {}
    pub const LLM_PROMPTS: &str = "llmPrompts";
    /// Fetch the consolidation source text (un-consolidated working content). params: { sessionId, projectScope }
    pub const CONSOLIDATE_SOURCE: &str = "consolidateSource";
}

/// hello request params
#[derive(Debug, Serialize, Deserialize)]
pub struct HelloParams {
    pub version: u32,
}

/// hello response result
#[derive(Debug, Serialize, Deserialize)]
pub struct HelloResult {
    pub version: u32,
    pub name: String,
}

/// Standard error codes
pub mod codes {
    /// Requested method does not exist
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid params
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal error
    pub const INTERNAL_ERROR: i32 = -32603;
    /// Protocol version mismatch
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

