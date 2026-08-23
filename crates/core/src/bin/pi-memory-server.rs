//! pi-memory resident server: stdio JSON-RPC entry point.
//!
//! Single process, multiple databases: one DB file per projectScope (physical isolation),
//! a single globally shared Embedder (the model is loaded once), and a per-project vector index.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use pi_memory_core::embedding_runtime::{ProjectVectors, SharedEmbedder};
use pi_memory_core::paths;
use pi_memory_core::protocol::{
    codes, methods, HelloParams, HelloResult, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
use pi_memory_core::storage::{Storage, TursoStorage};

/// Per-project runtime unit: storage + vectors (isolated per scope).
struct ProjectRuntime {
    storage: TursoStorage,
    vectors: ProjectVectors,
}

/// Pool of project runtimes (accessed serially from a single thread).
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

    /// Get (or lazily open) the runtime for a project.
    fn get_or_open(&mut self, project_scope: &str) -> anyhow::Result<&mut ProjectRuntime> {
        if !self.projects.contains_key(project_scope) {
            let db_path = paths::ensure_project_db_dir(&self.data_dir, project_scope)?;
            let storage = TursoStorage::open(db_path.to_str().expect("path must be UTF-8"))?;
            let mut vectors = ProjectVectors::new()?;
            vectors.hydrate(&storage)?;
            pi_memory_core::debug_log(&format!("opened project db: {db_path:?}"));
            self.projects.insert(project_scope.to_string(), ProjectRuntime { storage, vectors });
        }
        Ok(self.projects.get_mut(project_scope).expect("just inserted"))
    }

    /// Number of open projects (diagnostics).
    fn open_project_count(&self) -> usize {
        self.projects.len()
    }
}

fn main() {
    // 单例模式：--socket <path> 时监听 unix socket 供多个 pi 会话复用同一进程；
    // 缺省时保持 stdio 模式（测试/调试用）。
    let socket_path = std::env::args()
        .position(|a| a == "--socket")
        .and_then(|i| std::env::args().nth(i + 1));
    if let Some(path) = socket_path {
        #[cfg(unix)]
        {
            return run_socket_server(&path);
        }
        #[cfg(not(unix))]
        {
            // 非 unix 平台回退 stdio 模式（当前仅发布 darwin/linux）
            eprintln!("[pi-memory-rust] socket mode not supported here, falling back to stdio");
        }
    }

    let shared = SharedEmbedder::new();
    let mut pool = StoragePool::new();

    if !shared.model_ready() {
        // Model not ready: download in a background thread (does not block the RPC loop),
        // or trigger manually via /memory prepare-model
        let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
        let shared_for_download = shared.clone();
        std::thread::spawn(move || {
            match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                Ok(()) => {
                    pi_memory_core::debug_log(&format!("model downloaded in background: {}", model_dir.display()));
                    shared_for_download.warmup();
                }
                Err(e) => {
                    pi_memory_core::debug_log(&format!(
                        "background model download failed (retry later via /memory prepare-model): {e}"
                    ))
                }
            }
        });
    } else {
        shared.warmup();
    }

    serve(&mut pool, &shared);
}

/// 单例模式共享状态：全局锁串行化所有 RPC（单请求 <10ms，跨连接串行开销可忽略）。
struct SharedState {
    pool: StoragePool,
    shared: SharedEmbedder,
}

/// 单例模式：监听 unix socket，多个 pi 会话的连接复用同一进程（模型只加载一份）。
/// 生命周期：显式 shutdown（且无其他连接）立即退出；空闲 60s（无任何连接）超时退出防孤儿；
/// socket 文件退出时清理。
#[cfg(unix)]
fn run_socket_server(socket_path: &str) {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let _ = std::fs::remove_file(socket_path);
    // 确保 socket 父目录存在（~/.local/share/pi-memory-rust 可能未创建）
    if let Some(parent) = std::path::Path::new(socket_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[pi-memory-rust] failed to bind socket {socket_path}: {e}");
            std::process::exit(1);
        }
    };
    // 非阻塞 accept：空闲检测 + 退出判断
    if let Err(e) = listener.set_nonblocking(true) {
        eprintln!("[pi-memory-rust] failed to set socket non-blocking: {e}");
        std::process::exit(1);
    }

    let state = Arc::new(Mutex::new(SharedState {
        pool: StoragePool::new(),
        shared: SharedEmbedder::new(),
    }));
    let conn_count = Arc::new(AtomicUsize::new(0));
    let exit_flag = Arc::new(AtomicBool::new(false));

    // 模型预热（与 stdio 模式一致）
    {
        let shared = &state.lock().expect("mutex poisoned").shared;
        if !shared.model_ready() {
            let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
            let shared_for_download = shared.clone();
            std::thread::spawn(move || {
                match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                    Ok(()) => {
                        pi_memory_core::debug_log(&format!(
                            "model downloaded in background: {}",
                            model_dir.display()
                        ));
                        shared_for_download.warmup();
                    }
                    Err(e) => pi_memory_core::debug_log(&format!(
                        "background model download failed (retry later via /memory prepare-model): {e}"
                    )),
                }
            });
        } else {
            shared.warmup();
        }
    }

    let idle_timeout = std::time::Duration::from_secs(60);
    let mut idle_start: Option<std::time::Instant> = None;
    pi_memory_core::debug_log(&format!("socket server listening on {socket_path}"));
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                idle_start = None; // 有连接：重置空闲计时
                conn_count.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&state);
                let conn_count = Arc::clone(&conn_count);
                let exit_flag = Arc::clone(&exit_flag);
                std::thread::spawn(move || {
                    handle_socket_conn(stream, state, exit_flag);
                    conn_count.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if exit_flag.load(Ordering::Relaxed) && conn_count.load(Ordering::SeqCst) == 0 {
                    break; // 显式 shutdown 且无其他连接
                }
                if conn_count.load(Ordering::SeqCst) == 0 {
                    let now = std::time::Instant::now();
                    idle_start = Some(idle_start.unwrap_or(now));
                    if now.duration_since(idle_start.unwrap()) >= idle_timeout {
                        pi_memory_core::debug_log("socket server idle, exiting");
                        break; // 空闲超时：防孤儿进程
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(_) => break,
        }
    }

    // 等待在途连接处理完（shutdown 后）
    while conn_count.load(Ordering::SeqCst) > 0 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // 等待后台模型加载结束再退出（避免与 ONNX 全局初始化竞态）
    state
        .lock()
        .expect("mutex poisoned")
        .shared
        .wait_loading(std::time::Duration::from_millis(800));
    let _ = std::fs::remove_file(socket_path);
}

/// 处理单个 socket 连接：读帧 → 串行执行 RPC → 写响应；shutdown 置退出标志并关闭该连接。
#[cfg(unix)]
fn handle_socket_conn(
    stream: std::os::unix::net::UnixStream,
    state: Arc<Mutex<SharedState>>,
    exit_flag: Arc<AtomicBool>,
) {
    use std::sync::atomic::Ordering;

    // accept 的 stream 继承 listener 的非阻塞标志，恢复阻塞读以免 EAGAIN 误判断开
    if let Err(e) = stream.set_nonblocking(false) {
        eprintln!("[pi-memory-rust] failed to set stream blocking: {e}");
        return;
    }
    let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone stream"));
    let mut writer = std::io::BufWriter::new(stream);
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break, // 客户端断开
            Err(e) => {
                eprintln!("[pi-memory-rust] read failed: {e}");
                break;
            }
        };
        let request: RpcRequest = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[pi-memory-rust] request parse failed: {e}");
                continue;
            }
        };
        let shutdown = request.method == methods::SHUTDOWN;
        let mut guard = state.lock().expect("mutex poisoned");
        let SharedState { pool, shared } = &mut *guard;
        let payload = handle_request(&request, pool, shared);
        drop(guard);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] write failed: {e}");
            break;
        }
        if shutdown {
            exit_flag.store(true, Ordering::SeqCst);
            break;
        }
    }
}

fn serve(pool: &mut StoragePool, shared: &SharedEmbedder) {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = std::io::BufWriter::new(stdout.lock());

    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break, // stdin EOF: host closed
            Err(e) => {
                eprintln!("[pi-memory-rust] read failed: {e}");
                break;
            }
        };
        let request: RpcRequest = match serde_json::from_str(&frame) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[pi-memory-rust] request parse failed: {e}");
                continue;
            }
        };
        let shutdown = request.method == methods::SHUTDOWN;
        let payload = handle_request(&request, pool, shared);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] write failed: {e}");
            break;
        }
        if shutdown {
            // Bounded wait for the background model load before exiting (avoids SIGSEGV from
            // racing ONNX global init); give up on timeout — only happens when exiting right
            // after startup, so normal shutdown stays fast
            shared.wait_loading(std::time::Duration::from_millis(800));
            break;
        }
    }
}

/// Handle a single request and return the response JSON
fn handle_request(request: &RpcRequest, pool: &mut StoragePool, shared: &SharedEmbedder) -> serde_json::Value {
    let response = match request.method.as_str() {
        methods::HELLO => {
            let params: HelloParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(_) => {
                    return serde_json::to_value(RpcResponse::err(
                        request.id,
                        codes::INVALID_PARAMS,
                        "invalid hello params",
                    ))
                    .expect("serialization cannot fail")
                }
            };
            if params.version != PROTOCOL_VERSION {
                RpcResponse::err(
                    request.id,
                    codes::VERSION_MISMATCH,
                    format!("protocol version mismatch: client {}, server {}", params.version, PROTOCOL_VERSION),
                )
            } else {
                RpcResponse::ok(
                    request.id,
                    serde_json::to_value(HelloResult {
                        version: PROTOCOL_VERSION,
                        name: "pi-memory-rust".into(),
                    })
                    .expect("serialization cannot fail"),
                )
            }
        }
        methods::DIAGNOSE => {
            // Diagnose the current project (optional projectScope param)
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
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid buildRecallInjection params"),
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
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid retainTurnMemories params"),
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
                            serde_json::to_value(result).expect("serialization cannot fail"),
                        ),
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => {
                    RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid consolidateSessionMemories params")
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
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid consolidateSource params"),
            }
        }
        methods::MEMORY_COMMAND => {
            match serde_json::from_value::<MemoryCommandParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => {
                        let result = pi_memory_core::runtime::memory_command(
                            &runtime.storage,
                            &params.sub,
                            &params.target,
                            &params.project_scope,
                            &params.session_id,
                            &shared.status(),
                        );
                        match result {
                            Ok(text) => {
                                // clear only wipes the DB; rebuild the in-memory vector index so
                                // recall never injects deleted memories
                                if params.sub == "clear" {
                                    match ProjectVectors::new() {
                                        Ok(vectors) => runtime.vectors = vectors,
                                        Err(e) => {
                                            return serde_json::to_value(RpcResponse::err(
                                                request.id,
                                                codes::INTERNAL_ERROR,
                                                format!("failed to rebuild vector index: {e}"),
                                            ))
                                            .expect("serialization cannot fail")
                                        }
                                    }
                                }
                                RpcResponse::ok(request.id, serde_json::json!(text))
                            }
                            Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                        }
                    }
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid memoryCommand params"),
            }
        }
        methods::MIGRATE => {
            match serde_json::from_value::<MigrateParams>(request.params.clone()) {
                Ok(params) => match pool.get_or_open(&params.project_scope) {
                    Ok(runtime) => match runtime.storage.migrate_from(&params.source_path) {
                        Ok(stats) => {
                            RpcResponse::ok(request.id, serde_json::to_value(stats).expect("serialization cannot fail"))
                        }
                        Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                    },
                    Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
                },
                Err(_) => RpcResponse::err(request.id, codes::INVALID_PARAMS, "invalid migrate params"),
            }
        }
        methods::PREPARE_MODEL => {
            let model_dir = pi_memory_core::embedding_runtime::default_model_dir();
            match pi_memory_core::model_download::ensure_model_dir(&model_dir) {
                Ok(()) => {
                    shared.warmup();
                    RpcResponse::ok(request.id, serde_json::json!("model ready"))
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
            // The main loop exits after responding
            RpcResponse::ok(request.id, serde_json::json!({ "bye": true }))
        }
        _ => RpcResponse::err(request.id, codes::METHOD_NOT_FOUND, "unknown method"),
    };
    serde_json::to_value(response).expect("serialization cannot fail")
}

// ---------------------------------------------------------------------------
// RPC param types
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
// Frame protocol
// ---------------------------------------------------------------------------

/// Read a single frame: `<u32 big-endian length><JSON>`
fn read_frame(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Write a frame
fn write_frame(writer: &mut impl Write, payload: &str) -> std::io::Result<()> {
    let bytes = payload.as_bytes();
    let len = u32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too long"))?;
    writer.write_all(&len.to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()
}

// Keep Arc referenced for type consistency (SharedEmbedder already uses Arc internally)
#[allow(dead_code)]
fn _keep_arc(_: &Arc<SharedEmbedder>) {}

#[cfg(test)]
mod tests {
    use super::{read_frame, write_frame};
    use std::io::Cursor;

    /// 确定性伪随机：同一种子生成同样的字节序列，保证可复现。
    fn seeded_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 0xFF) as u8
            })
            .collect()
    }

    #[test]
    fn read_frame_tolerates_malformed_input() {
        // 各种畸形输入必须返回 Ok/Err，绝不 panic
        // 1) 空输入 → Ok(None)
        assert!(read_frame(&mut &b""[..]).unwrap().is_none());
        // 2) 只读 1-3 字节长度前缀（截断）→ Ok(None)
        assert!(read_frame(&mut &b"\x00"[..]).unwrap().is_none());
        assert!(read_frame(&mut &b"\x00\x00"[..]).unwrap().is_none());
        assert!(read_frame(&mut &b"\x00\x00\x00"[..]).unwrap().is_none());
        // 3) 长度前缀完整但帧体截断 → Err(UnexpectedEof)（与长度前缀截断的 Ok(None) 语义区分；
        //    两者都会让 serve 循环退出，Err 标记异常流）
        assert!(read_frame(&mut &b"\x00\x00\x00\x05ab"[..]).is_err());
        // 4) 超大帧（>16MB）→ Err
        assert!(read_frame(&mut &b"\xff\xff\xff\xff"[..]).is_err());
        assert!(read_frame(&mut &b"\x01\x00\x00\x00"[..]).is_err());
        // 5) 零长度帧 → Ok(Some(""))
        assert_eq!(read_frame(&mut &b"\x00\x00\x00\x00"[..]).unwrap(), Some(String::new()));
        // 6) 非 UTF-8 帧体 → 不 panic（lossy 替换）
        let raw = [0u8, 0, 0, 1, 0xFF];
        assert!(read_frame(&mut &raw[..]).is_ok());
        // 7) 确定性 fuzz：1000 组随机字节不 panic
        for seed in 0..1000u64 {
            let data = seeded_bytes(seed, (seed % 64) as usize);
            let _ = read_frame(&mut &data[..]);
        }
        // 8) 畸形负载 + 随机长度前缀组合
        for seed in 0..500u64 {
            let data = seeded_bytes(seed, 8);
            let len = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            let _ = read_frame(&mut &data[..]);
            let _ = std::hint::black_box(len);
        }
    }

    #[test]
    fn frame_roundtrip_and_max_length() {
        // 正常往返
        let mut buf = Vec::new();
        write_frame(&mut buf, r#"{"id":1,"method":"hello"}"#).unwrap();
        let parsed = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(parsed, r#"{"id":1,"method":"hello"}"#);
        // 超大 payload 写入报错（>u32）
        let huge = "x".repeat(u32::MAX as usize + 8);
        let mut buf = Vec::new();
        assert!(write_frame(&mut buf, &huge).is_err());
        // 混合帧流：多帧连续解析
        let mut stream = Vec::new();
        for i in 0..10 {
            let payload = format!(r#"{{"id":{i}}}"#);
            write_frame(&mut stream, &payload).unwrap();
        }
        let mut cursor = Cursor::new(stream);
        for i in 0..10 {
            let frame = read_frame(&mut cursor).unwrap().unwrap();
            assert_eq!(frame, format!(r#"{{"id":{i}}}"#));
        }
    }
}
