//! pi-memory 常驻服务：stdio JSON-RPC 入口。
//!
//! 单进程多库：按 projectScope 打开独立数据库文件（物理隔离），
//! Embedder 全局共享（模型只加载一份），向量索引按项目独立。

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use pi_memory_core::embedding_runtime::{ProjectVectors, SharedEmbedder};
use pi_memory_core::paths;
use pi_memory_core::protocol::{
    codes, methods, HelloParams, HelloResult, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
use pi_memory_core::storage::{Storage, TursoStorage};

/// 项目运行时单元：存储 + 向量（按 scope 独立）。
struct ProjectRuntime {
    storage: TursoStorage,
    vectors: ProjectVectors,
}

/// 项目运行时池（单线程串行访问）。
struct StoragePool {
    data_dir: PathBuf,
    projects: HashMap<String, ProjectRuntime>,
}

impl StoragePool {
    fn new() -> Self {
        Self {
            data_dir: paths::default_data_dir(),
            projects: HashMap::new(),
        }
    }

    /// 获取（或首次打开）某项目的运行时。
    fn get_or_open(&mut self, project_scope: &str) -> anyhow::Result<&mut ProjectRuntime> {
        if !self.projects.contains_key(project_scope) {
            let db_path = paths::ensure_project_db_dir(&self.data_dir, project_scope)?;
            let storage = TursoStorage::open(db_path.to_str().expect("路径必须为 UTF-8"))?;
            let vectors = ProjectVectors::new()?;
            eprintln!("[pi-memory-rust] 打开项目库: {db_path:?}");
            self.projects.insert(project_scope.to_string(), ProjectRuntime { storage, vectors });
        }
        Ok(self.projects.get_mut(project_scope).expect("刚插入必有"))
    }

    /// 已打开的项目数（诊断用）。
    fn open_project_count(&self) -> usize {
        self.projects.len()
    }
}

fn main() {
    let shared = SharedEmbedder::new();
    let mut pool = StoragePool::new();

    if !shared.model_ready() {
        // 模型未就绪：后台线程下载（不阻塞 RPC 主循环），或 /memory prepare-model 手动触发
        let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
        std::thread::spawn(move || {
            match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                Ok(()) => eprintln!("[pi-memory-rust] 模型后台下载完成: {}", model_dir.display()),
                Err(e) => eprintln!("[pi-memory-rust] 模型后台下载失败（可稍后 /memory prepare-model 重试）: {e}"),
            }
        });
    }

    serve(&mut pool, &shared);
}

fn serve(pool: &mut StoragePool, shared: &SharedEmbedder) {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break, // stdin EOF：宿主已关闭
            Err(e) => {
                eprintln!("[pi-memory-rust] 读取失败: {e}");
                break;
            }
        };
        let request: RpcRequest = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[pi-memory-rust] 请求解析失败: {e}");
                continue;
            }
        };
        let shutdown = request.method == methods::SHUTDOWN;
        let payload = handle_request(&request, pool, shared);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] 写入失败: {e}");
            break;
        }
        if shutdown {
            break;
        }
    }
}

/// 处理单个请求，返回响应 JSON
fn handle_request(request: &RpcRequest, pool: &mut StoragePool, shared: &SharedEmbedder) -> serde_json::Value {
    let response = match request.method.as_str() {
        methods::HELLO => {
            let params: HelloParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(_) => {
                    return serde_json::to_value(RpcResponse::err(
                        request.id,
                        codes::INVALID_PARAMS,
                        "hello 参数无效",
                    ))
                    .expect("序列化不会失败")
                }
            };
            if params.version != PROTOCOL_VERSION {
                RpcResponse::err(
                    request.id,
                    codes::VERSION_MISMATCH,
                    format!("协议版本不匹配：客户端 {}，服务端 {}", params.version, PROTOCOL_VERSION),
                )
            } else {
                RpcResponse::ok(
                    request.id,
                    serde_json::to_value(HelloResult {
                        version: PROTOCOL_VERSION,
                        name: "pi-memory-rust".into(),
                    })
                    .expect("序列化不会失败"),
                )
            }
        }
        methods::DIAGNOSE => {
            // 诊断当前项目（可选 projectScope 参数）
            let scope = request
                .params
                .get("projectScope")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            match pool.get_or_open(scope) {
                Ok(runtime) => {
                    let mut info = runtime.storage.diagnose().unwrap_or_else(|e| {
                        serde_json::json!({ "backend": "turso", "error": e.to_string() })
                    });
                    info["project"] = serde_json::json!(scope);
                    info["embedding"] = serde_json::json!(shared.status());
                    info["open_projects"] = serde_json::json!(pool.open_project_count());
                    RpcResponse::ok(request.id, info)
                }
                Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
            }
        }
        methods::BUILD_RECALL_INJECTION => {
            match serde_json::from_value::<RecallParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => match pi_memory_core::runtime::build_recall_injection(
                        &runtime.storage,
                        &params.prompt,
                        &params.project_scope,
                        &params.session_id,
                        shared,
                        &mut runtime.vectors,
                    ) {
                        Ok(text) => RpcResponse::ok(request.id, serde_json::json!(text)),
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "buildRecallInjection 参数无效"),
            }
        }
        methods::RETAIN_TURN_MEMORIES => {
            match serde_json::from_value::<RetainParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => match pi_memory_core::runtime::retain_turn_memories(
                        &runtime.storage,
                        params.turn_index,
                        &params.text,
                        &params.session_id,
                        &params.project_scope,
                        shared,
                        &mut runtime.vectors,
                        params.llm_result.as_deref(),
                    ) {
                        Ok(()) => RpcResponse::ok(request.id, serde_json::json!(null)),
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "retainTurnMemories 参数无效"),
            }
        }
        methods::CONSOLIDATE_SESSION_MEMORIES => {
            match serde_json::from_value::<ConsolidateParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => match pi_memory_core::runtime::consolidate_session_memories(
                        &runtime.storage,
                        &params.session_id,
                        &params.project_scope,
                        params.llm_result.as_deref(),
                        params.llm_failed,
                    ) {
                        Ok(result) => RpcResponse::ok(
                            request.id,
                            serde_json::to_value(result).expect("序列化不会失败"),
                        ),
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => {
                    RpcResponse::err(request.id, codes::INVALID_PARAMS, "consolidateSessionMemories 参数无效")
                }
            }
        }
        methods::CONSOLIDATE_SOURCE => {
            match serde_json::from_value::<ConsolidateParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let text = pi_memory_core::runtime::consolidate_source_text(
                            &runtime.storage, &params.session_id, &params.project_scope,
                        );
                        RpcResponse::ok(request.id, serde_json::json!(text))
                    }
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "consolidateSource 参数无效"),
            }
        }
        methods::MEMORY_COMMAND => {
            match serde_json::from_value::<MemoryCommandParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => match pi_memory_core::runtime::memory_command(
                        &runtime.storage,
                        &params.sub,
                        &params.target,
                        &params.project_scope,
                        &params.session_id,
                        &shared.status(),
                    ) {
                        Ok(text) => RpcResponse::ok(request.id, serde_json::json!(text)),
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "memoryCommand 参数无效"),
            }
        }
        methods::MIGRATE => {
            match serde_json::from_value::<MigrateParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => match runtime.storage.migrate_from(&params.source_path) {
                        Ok(stats) => {
                            RpcResponse::ok(request.id, serde_json::to_value(stats).expect("序列化不会失败"))
                        }
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "migrate 参数无效"),
            }
        }
        methods::PREPARE_MODEL => {
            let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
            match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                Ok(()) => {
                    shared.ensure_loaded();
                    RpcResponse::ok(request.id, serde_json::json!("模型就绪"))
                }
                Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
            }
        }
        methods::LLM_PROMPTS => RpcResponse::ok(
            request.id,
            serde_json::json!({
                "extract": pi_memory_core::llm::EXTRACT_PROMPT,
                "consolidate": pi_memory_core::llm::CONSOLIDATE_PROMPT,
            }),
        ),
        methods::SHUTDOWN => {
            // 由主循环在响应后退出
            RpcResponse::ok(request.id, serde_json::json!({ "bye": true }))
        }
        _ => RpcResponse::err(request.id, codes::METHOD_NOT_FOUND, "未知方法"),
    };
    serde_json::to_value(response).expect("序列化不会失败")
}

// ---------------------------------------------------------------------------
// RPC 参数类型
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct RecallParams {
    prompt: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "sessionId")]
    session_id: String,
}

#[derive(serde::Deserialize)]
struct RetainParams {
    #[serde(rename = "turnIndex")]
    turn_index: i64,
    text: String,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "llmResult", default)]
    llm_result: Option<String>,
}

#[derive(serde::Deserialize)]
struct ConsolidateParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "llmResult", default)]
    llm_result: Option<String>,
    #[serde(rename = "llmFailed", default)]
    llm_failed: bool,
}

#[derive(serde::Deserialize)]
struct MigrateParams {
    #[serde(rename = "sourcePath")]
    source_path: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
}

#[derive(serde::Deserialize)]
struct MemoryCommandParams {
    sub: String,
    #[serde(default)]
    target: String,
    #[serde(rename = "projectScope")]
    project_scope: String,
    #[serde(rename = "sessionId")]
    session_id: String,
}

// ---------------------------------------------------------------------------
// 帧协议
// ---------------------------------------------------------------------------

/// 单次读取一帧：`<u32 大端长度><JSON>`
fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "帧过大"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// 写一帧
fn write_frame(writer: &mut impl Write, payload: &str) -> std::io::Result<()> {
    let bytes = payload.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "帧超长"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

// 引用 Arc 保持类型一致性（SharedEmbedder 内部已用 Arc）
#[allow(dead_code)]
fn _keep_arc(_: &Arc<SharedEmbedder>) {}
