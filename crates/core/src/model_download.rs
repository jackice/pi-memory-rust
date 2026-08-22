//! Model download: all-MiniLM-L6-v2 (sentence-transformers) auto-fetch.
//!
//! Strategy: official source → mirror (hf-mirror.com) → local path (PI_MEMORY_MODEL_PATH).
//! Download failures do not block the main flow (embedding degrades to FTS-only).

use anyhow::{Context, Result};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Official and mirror base URLs (model repo root)
const HF_OFFICIAL: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main";
const HF_MIRROR: &str = "https://hf-mirror.com/sentence-transformers/all-MiniLM-L6-v2/resolve/main";

/// File manifest: remote relative path → local file name
const REMOTE_FILES: [(&str, &str); 2] = [
    ("onnx/model.onnx", "model.onnx"),
    ("tokenizer.json", "tokenizer.json"),
];

/// Whether the model directory is complete.
pub fn model_dir_complete(dir: &std::path::Path) -> bool {
    dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists()
}

/// Ensure the model directory is usable: returns immediately if complete, otherwise downloads missing files.
/// Returns Err when every source fails (caller degrades).
pub fn ensure_model_dir(dir: &std::path::Path) -> Result<()> {
    if model_dir_complete(dir) {
        return Ok(());
    }
    static DOWNLOAD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = DOWNLOAD_LOCK.get_or_init(|| Mutex::new(())).lock().expect("download lock poisoned");
    if model_dir_complete(dir) {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create model dir: {}", dir.display()))?;

    let mut failures = Vec::new();
    for (remote, local) in REMOTE_FILES {
        let target = dir.join(local);
        if target.exists() {
            continue;
        }
        match download_with_fallback(remote, &target) {
            Ok(()) => {}
            Err(e) => failures.push(format!("{local}: {e}")),
        }
    }
    if !model_dir_complete(dir) {
        anyhow::bail!(
            "model download incomplete ({}). Place model.onnx/tokenizer.json in {} manually, or point PI_MEMORY_MODEL_PATH at a complete directory",
            failures.join("; "),
            dir.display()
        );
    }
    Ok(())
}

fn download_with_fallback(remote: &str, target: &std::path::Path) -> Result<()> {
    let mirror = std::env::var("PI_MEMORY_HF_MIRROR").unwrap_or_else(|_| HF_MIRROR.to_string());
    for base in [HF_OFFICIAL, &mirror] {
        let url = format!("{base}/{remote}");
        match download_file(&url, target) {
            Ok(()) => {
                crate::debug_log(&format!("model file downloaded: {url}"));
                return Ok(());
            }
            Err(e) => {
                crate::debug_log(&format!("download failed (trying next source) {url}: {e}"));
            }
        }
    }
    anyhow::bail!("all sources failed")
}

fn download_file(url: &str, target: &std::path::Path) -> Result<()> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .build(),
    );
    let response = agent.get(url).call().with_context(|| format!("HTTP request failed: {url}"))?;
    let mut body = Vec::new();
    let mut reader = std::io::Read::take(response.into_body().into_reader(), 512 * 1024 * 1024); // 512MB cap
    std::io::Read::read_to_end(&mut reader, &mut body).with_context(|| "failed to read response")?;
    let tmp = target.with_extension("part");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}
