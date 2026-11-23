#![cfg(unix)]

//! Process-level coverage for socket singleton startup and socket permissions.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn request_shutdown(stream: &mut UnixStream) {
    let payload = br#"{"id":1,"method":"shutdown","params":{}}"#;
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(payload).unwrap();
    stream.flush().unwrap();

    let mut len = [0_u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut response = vec![0_u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut response).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["bye"], true);
}

#[test]
fn simultaneous_socket_startup_keeps_one_listener_and_secures_paths() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let base = std::path::Path::new("/tmp").join(format!("pm-sock-{}-{nonce}", std::process::id()));
    let data_dir = base.join("data");
    let socket_dir = base.join("sock");
    std::fs::create_dir_all(&data_dir).unwrap();
    let socket_path = socket_dir.join("s.sock");

    let launch = || {
        ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_pi-memory-server"))
                .arg("--socket")
                .arg(&socket_path)
                .env("PI_MEMORY_DATA_DIR", &data_dir)
                // A custom model path without a manifest deliberately prevents warmup from
                // attempting a download; socket startup is independent of model availability.
                .env("PI_MEMORY_MODEL_PATH", base.join("missing-model"))
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    };
    let mut children = vec![launch(), launch()];

    let deadline = Instant::now() + Duration::from_secs(10);
    let winner_index = loop {
        let listener_ready = UnixStream::connect(&socket_path).is_ok();
        let alive = children
            .iter_mut()
            .enumerate()
            .filter_map(|(index, child)| {
                child.0.try_wait().ok().flatten().is_none().then_some(index)
            })
            .collect::<Vec<_>>();
        if listener_ready && alive.len() == 1 {
            break alive[0];
        }
        if alive.is_empty() || Instant::now() >= deadline {
            let diagnostics = children
                .iter_mut()
                .map(|child| {
                    let status = child.0.try_wait().unwrap();
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.0.stderr.take() {
                        let _ = pipe.read_to_string(&mut stderr);
                    }
                    format!("status={status:?}, stderr={stderr}")
                })
                .collect::<Vec<_>>()
                .join("; ");
            if diagnostics.contains("Operation not permitted") {
                eprintln!(
                    "skipping socket process test: sandbox denied AF_UNIX bind ({diagnostics})"
                );
                drop(children);
                let _ = std::fs::remove_dir_all(&base);
                return;
            }
            panic!(
                "socket startup did not settle to one live server (alive={alive:?}; {diagnostics})"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let socket_dir_mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode() & 0o777;
    let socket_mode = std::fs::metadata(&socket_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let lock_path = socket_path.with_extension("lock");
    let lock_mode = std::fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(socket_dir_mode, 0o700);
    assert_eq!(socket_mode, 0o600);
    assert_eq!(lock_mode, 0o600);

    let mut stream = UnixStream::connect(&socket_path).expect("winning server remains reachable");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    request_shutdown(&mut stream);
    drop(stream);

    let shutdown_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if children[winner_index].0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < shutdown_deadline,
            "server did not exit after shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !socket_path.exists(),
        "winning server removes its socket on shutdown"
    );

    drop(children);
    let _ = std::fs::remove_dir_all(base);
}
