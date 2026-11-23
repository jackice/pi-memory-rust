//! Model download: all-MiniLM-L6-v2 (sentence-transformers) auto-fetch.
//!
//! Strategy: official source → mirror (hf-mirror.com) → local path (PI_MEMORY_MODEL_PATH).
//! Download failures do not block the main flow (embedding degrades to FTS-only).

use anyhow::{Context, Result};
use sha2::Digest;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Official and mirror base URLs (model repo root)
/// Whether the model directory is complete.
pub fn model_dir_complete(dir: &std::path::Path) -> bool {
    crate::model_manifest::verify_dir(dir).is_ok()
}

/// Ensure the model directory is usable: returns immediately if complete, otherwise downloads missing files.
/// Returns Err when every source fails (caller degrades).
pub fn ensure_model_dir(dir: &std::path::Path) -> Result<()> {
    let manifest = crate::model_manifest::expected_manifest(dir)?;
    if crate::model_manifest::verify_dir(dir).is_ok() {
        return Ok(());
    }
    static DOWNLOAD_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = DOWNLOAD_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("download lock poisoned");
    if crate::model_manifest::verify_dir(dir).is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create model dir: {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let mut failures = Vec::new();
    for file in &manifest.files {
        let target = dir.join(&file.name);
        if crate::model_manifest::verify_dir(dir).is_ok() {
            continue;
        }
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        match download_with_fallback(&manifest, file, &target) {
            Ok(()) => {}
            Err(e) => failures.push(format!("{}: {e}", file.name)),
        }
    }
    if crate::model_manifest::verify_dir(dir).is_err() {
        anyhow::bail!(
            "model download incomplete or invalid ({}). Place manifest-matched files in {}, or set PI_MEMORY_MODEL_PATH and PI_MEMORY_MODEL_MANIFEST",
            failures.join("; "),
            dir.display()
        );
    }
    Ok(())
}

fn download_with_fallback(
    manifest: &crate::model_manifest::ModelManifest,
    file: &crate::model_manifest::ModelFile,
    target: &std::path::Path,
) -> Result<()> {
    let mirror = std::env::var("PI_MEMORY_HF_MIRROR")
        .unwrap_or_else(|_| "https://hf-mirror.com".to_string());
    let urls = [
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            manifest.repository, manifest.revision, file.remote
        ),
        format!(
            "{}/{}/resolve/{}/{}",
            mirror.trim_end_matches('/'),
            manifest.repository,
            manifest.revision,
            file.remote
        ),
    ];
    for url in urls {
        match download_file(&url, target, file) {
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

fn download_file(
    url: &str,
    target: &std::path::Path,
    expected: &crate::model_manifest::ModelFile,
) -> Result<()> {
    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);
    let temp_id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let target_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("model");
    let tmp = target.with_file_name(format!(
        "{target_name}.{}.{}.part",
        std::process::id(),
        temp_id
    ));
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .timeout_connect(Some(Duration::from_secs(15)))
            .build(),
    );
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("HTTP request failed: {url}"))?;
    let verify_and_install = (|| -> Result<()> {
        let mut reader = response.into_body().into_reader();
        // create_new is atomic and refuses to follow a pre-existing symlink at the
        // predictable temporary path. The model directory is private, but this also
        // protects user-managed/custom model directories from stale or planted .part files.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        let mut hasher = sha2::Sha256::new();
        let mut buffer = [0u8; 1024 * 1024];
        let mut size = 0u64;
        loop {
            let count = std::io::Read::read(&mut reader, &mut buffer)?;
            if count == 0 {
                break;
            }
            size += count as u64;
            anyhow::ensure!(size <= 512 * 1024 * 1024, "download exceeds 512 MiB limit");
            std::io::Write::write_all(&mut file, &buffer[..count])?;
            hasher.update(&buffer[..count]);
        }
        file.sync_all()?;
        anyhow::ensure!(
            size == expected.size,
            "downloaded size mismatch for {}: expected {}, got {}",
            expected.name,
            expected.size,
            size
        );
        let actual = format!("{:x}", hasher.finalize());
        anyhow::ensure!(
            actual == expected.sha256,
            "downloaded SHA-256 mismatch for {}: expected {}, got {}",
            expected.name,
            expected.sha256,
            actual
        );
        std::fs::rename(&tmp, target)?;
        Ok(())
    })();
    if let Err(error) = verify_and_install {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
