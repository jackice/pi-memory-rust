//! pi-memory 常驻服务：stdio JSON-RPC 入口。
//!
//! 由 pi 扩展（TS 侧）spawn，生命周期由 TS 客户端管理（session_start 启动、
//! session_shutdown 关闭、崩溃自动重启）。重活全部在本进程内完成，
//! 只通过 stdio 与宿主交换小负载请求/响应。

use std::io::{BufRead, Write};

use pi_memory_core::protocol::{
    codes, methods, HelloParams, HelloResult, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
use pi_memory_core::embedding_runtime::EmbeddingRuntime;
use pi_memory_core::storage::{Storage, TursoStorage};

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
    let len = u32::try_from(bytes.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "帧超长")
    })?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

/// 处理单个请求，返回响应 JSON
fn handle_request(
    request: &RpcRequest,
    turso: &mut TursoStorage,
    embedding: &mut EmbeddingRuntime,
) -> serde_json::Value {
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
        methods::DIAGNOSE => match turso.diagnose() {
            Ok(mut info) => {
                info["embedding"] = serde_json::json!(embedding.status());
                RpcResponse::ok(request.id, info)
            }
            Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
        },
        methods::BUILD_RECALL_INJECTION => {
            match serde_json::from_value::<RecallParams>(request.params.clone()) {
                Ok(params) => match pi_memory_core::runtime::build_recall_injection(
                    turso,
                    &params.prompt,
                    &params.project_scope,
                    &params.session_id,
                    embedding,
                ) {
                    Ok(text) => RpcResponse::ok(request.id, serde_json::json!(text)),
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "buildRecallInjection 参数无效"),
            }
        }
        methods::RETAIN_TURN_MEMORIES => {
            match serde_json::from_value::<RetainParams>(request.params.clone()) {
                Ok(params) => match pi_memory_core::runtime::retain_turn_memories(
                    turso,
                    params.turn_index,
                    &params.text,
                    &params.session_id,
                    &params.project_scope,
                    embedding,
                    params.llm_result.as_deref(),
                ) {
                    Ok(()) => RpcResponse::ok(request.id, serde_json::json!(null)),
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "retainTurnMemories 参数无效"),
            }
        }
        methods::CONSOLIDATE_SESSION_MEMORIES => {
            match serde_json::from_value::<ConsolidateParams>(request.params.clone()) {
                Ok(params) => match pi_memory_core::runtime::consolidate_session_memories(
                    turso,
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
                Err(_) => {
                    RpcResponse::err(request.id, codes::INVALID_PARAMS, "consolidateSessionMemories 参数无效")
                }
            }
        }
        methods::CONSOLIDATE_SOURCE => {
            match serde_json::from_value::<ConsolidateParams>(request.params.clone()) {
                Ok(params) => {
                    let text = pi_memory_core::runtime::consolidate_source_text(
                        turso, &params.session_id, &params.project_scope,
                    );
                    RpcResponse::ok(request.id, serde_json::json!(text))
                }
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "consolidateSource 参数无效"),
            }
        }
        methods::LLM_PROMPTS => RpcResponse::ok(
            request.id,
            serde_json::json!({
                "extract": pi_memory_core::llm::EXTRACT_PROMPT,
                "consolidate": pi_memory_core::llm::CONSOLIDATE_PROMPT,
            }),
        ),
        methods::MIGRATE => {
            match serde_json::from_value::<MigrateParams>(request.params.clone()) {
                Ok(params) => match turso.migrate_from(&params.source_path) {
                    Ok(stats) => RpcResponse::ok(request.id, serde_json::to_value(stats).expect("序列化不会失败")),
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "migrate 参数无效"),
            }
        }
        methods::PREPARE_MODEL => {
            let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
            match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                Ok(()) => {
                    embedding.ensure_ready();
                    RpcResponse::ok(request.id, serde_json::json!("模型就绪"))
                }
                Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
            }
        }
        methods::MEMORY_COMMAND => {
            match serde_json::from_value::<MemoryCommandParams>(request.params.clone()) {
                Ok(params) => match pi_memory_core::runtime::memory_command(
                    turso,
                    &params.sub,
                    &params.target,
                    &params.project_scope,
                    &params.session_id,
                ) {
                    Ok(text) => RpcResponse::ok(request.id, serde_json::json!(text)),
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "memoryCommand 参数无效"),
            }
        }
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

fn main() {
    // 用法：pi-memory-server [--db <path>]，缺省用环境变量或临时路径
    let db_path = parse_db_path();
    let mut storage = match TursoStorage::open(&db_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[pi-memory-rust] 打开数据库失败 {db_path}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("[pi-memory-rust] 存储就绪: {db_path}");
    let mut embedding = EmbeddingRuntime::new();
    if !embedding.model_ready() {
        // 模型未就绪：后台线程下载（不阻塞 RPC 主循环），或用户 /memory prepare-model 手动触发
        let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
        std::thread::spawn(move || {
            match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                Ok(()) => eprintln!("[pi-memory-rust] 模型后台下载完成: {}", model_dir.display()),
                Err(e) => eprintln!("[pi-memory-rust] 模型后台下载失败（可稍后 /memory prepare-model 重试）: {e}"),
            }
        });
    }
    serve(&mut storage, &mut embedding);
}

/// 解析 db 路径：`--db <path>` > 环境变量 `PI_MEMORY_DB` > 默认路径
fn parse_db_path() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--db" {
            if let Some(path) = args.next() {
                return path;
            }
        }
    }
    if let Ok(path) = std::env::var("PI_MEMORY_DB") {
        return path;
    }
    let fallback = std::env::temp_dir().join("pi-memory-rust").join("default.db");
    if let Some(parent) = fallback.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    fallback.to_string_lossy().into_owned()
}

fn serve(turso: &mut TursoStorage, embedding: &mut EmbeddingRuntime) {
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
        let payload = handle_request(&request, turso, embedding);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] 写入失败: {e}");
            break;
        }
        if shutdown {
            break;
        }
    }
}
