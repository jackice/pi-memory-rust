//! pi-memory 常驻服务：stdio JSON-RPC 入口。
//!
//! 由 pi 扩展（TS 侧）spawn，生命周期由 TS 客户端管理（session_start 启动、
//! session_shutdown 关闭、崩溃自动重启）。重活全部在本进程内完成，
//! 只通过 stdio 与宿主交换小负载请求/响应。

use std::io::{BufRead, Write};

use pi_memory_core::protocol::{
    codes, methods, HelloParams, HelloResult, RpcRequest, RpcResponse, PROTOCOL_VERSION,
};
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
fn handle_request(request: &RpcRequest, storage: &mut dyn Storage) -> serde_json::Value {
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
        methods::DIAGNOSE => match storage.diagnose() {
            Ok(info) => RpcResponse::ok(request.id, info),
            Err(e) => RpcResponse::err(request.id, codes::INTERNAL_ERROR, e.to_string()),
        },
        methods::SHUTDOWN => {
            // 由主循环在响应后退出
            RpcResponse::ok(request.id, serde_json::json!({ "bye": true }))
        }
        _ => RpcResponse::err(request.id, codes::METHOD_NOT_FOUND, "未知方法"),
    };
    serde_json::to_value(response).expect("序列化不会失败")
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
    serve(&mut storage);
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
    std::env::temp_dir()
        .join("pi-memory-rust")
        .join("default.db")
        .to_string_lossy()
        .into_owned()
}

fn serve(storage: &mut dyn Storage) {
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
        let payload = handle_request(&request, storage);
        if let Err(e) = write_frame(&mut writer, &payload.to_string()) {
            eprintln!("[pi-memory-rust] 写入失败: {e}");
            break;
        }
        if shutdown {
            break;
        }
    }
}
