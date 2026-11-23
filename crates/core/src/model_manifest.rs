use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::Once;

pub const MANIFEST_JSON: &str = include_str!("../models.lock.json");

#[derive(Debug, Clone, Deserialize)]
pub struct ModelManifest {
    pub model_id: String,
    pub repository: String,
    pub revision: String,
    #[serde(default = "default_model_file")]
    pub model_file: String,
    #[serde(default = "default_tokenizer_file")]
    pub tokenizer_file: String,
    pub dimension: usize,
    pub max_length: usize,
    #[serde(default = "default_pooling")]
    pub pooling: String,
    #[serde(default)]
    pub query_prefix: String,
    #[serde(default)]
    pub document_prefix: String,
    pub files: Vec<ModelFile>,
}

fn default_pooling() -> String {
    "mean".into()
}

fn default_model_file() -> String {
    "model.onnx".into()
}

fn default_tokenizer_file() -> String {
    "tokenizer.json".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelFile {
    pub remote: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

pub fn hash_verification_disabled() -> bool {
    std::env::var("PI_MEMORY_MODEL_VERIFY").ok().as_deref() == Some("0")
}

pub fn hash_verification_disable_allowed() -> bool {
    cfg!(debug_assertions) || std::env::var("PI_MEMORY_ENV").ok().as_deref() == Some("development")
}

pub fn validate_verification_config() -> Result<bool> {
    let skip_hash = hash_verification_disabled();
    anyhow::ensure!(
        !skip_hash || hash_verification_disable_allowed(),
        "PI_MEMORY_MODEL_VERIFY=0 is only allowed in debug or PI_MEMORY_ENV=development builds"
    );
    if skip_hash {
        static WARN_ONCE: Once = Once::new();
        WARN_ONCE.call_once(|| {
            crate::debug_log(
                "WARNING: model content verification disabled (PI_MEMORY_MODEL_VERIFY=0)",
            );
        });
    }
    Ok(skip_hash)
}

pub fn expected_manifest(dir: &Path) -> Result<ModelManifest> {
    validate_verification_config()?;
    if let Ok(path) = std::env::var("PI_MEMORY_MODEL_MANIFEST") {
        anyhow::ensure!(
            Path::new(&path).is_absolute(),
            "PI_MEMORY_MODEL_MANIFEST must be an absolute path"
        );
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read model manifest {path}"))?;
        let manifest = parse_manifest_json(&raw).context("parse custom model manifest")?;
        validate_active_model(&manifest)?;
        return Ok(manifest);
    }
    if std::env::var_os("PI_MEMORY_MODEL_PATH").is_some() {
        anyhow::bail!("custom PI_MEMORY_MODEL_PATH requires PI_MEMORY_MODEL_MANIFEST");
    }
    let expected_dir = crate::embedding_runtime::default_model_dir();
    anyhow::ensure!(
        dir == expected_dir,
        "custom model directory requires PI_MEMORY_MODEL_MANIFEST"
    );
    let manifest = parse_manifest_json(MANIFEST_JSON).context("parse built-in model manifest")?;
    validate_active_model(&manifest)?;
    Ok(manifest)
}

/// Parse and structurally validate a manifest without requiring it to be the
/// currently selected runtime model. Used by CI to validate every candidate.
pub fn parse_manifest_json(raw: &str) -> Result<ModelManifest> {
    let manifest: ModelManifest = serde_json::from_str(raw).context("parse model manifest")?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn validate_active_model(manifest: &ModelManifest) -> Result<()> {
    anyhow::ensure!(
        manifest.model_id == crate::embedding_runtime::active_model_id(),
        "model manifest id does not match the active embedding model"
    );
    Ok(())
}

/// Validate every manifest field that can influence a filesystem path or URL
/// before callers create, remove, or download any files.
fn validate_manifest(manifest: &ModelManifest) -> Result<()> {
    anyhow::ensure!(!manifest.files.is_empty(), "model manifest has no files");
    anyhow::ensure!(
        manifest.files.len() <= 8,
        "model manifest contains more than 8 files"
    );
    let mut names = manifest
        .files
        .iter()
        .map(|file| file.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    anyhow::ensure!(
        names.windows(2).all(|pair| pair[0] != pair[1]),
        "model manifest contains duplicate file names"
    );
    anyhow::ensure!(
        is_safe_basename(&manifest.model_file)
            && is_safe_basename(&manifest.tokenizer_file)
            && manifest.model_file != manifest.tokenizer_file,
        "model and tokenizer files must be distinct safe basenames"
    );
    anyhow::ensure!(
        names.contains(&manifest.model_file.as_str())
            && names.contains(&manifest.tokenizer_file.as_str()),
        "model manifest must include the model_file and tokenizer_file"
    );
    anyhow::ensure!(
        manifest.dimension > 0
            && manifest.dimension <= 8192
            && manifest.max_length > 0
            && manifest.max_length <= 4096,
        "model manifest dimension or max_length is outside supported bounds"
    );
    anyhow::ensure!(
        !manifest.model_id.trim().is_empty(),
        "model_id must not be empty"
    );
    anyhow::ensure!(
        matches!(manifest.pooling.as_str(), "mean" | "cls"),
        "unsupported embedding pooling strategy: {}",
        manifest.pooling
    );
    anyhow::ensure!(
        manifest.query_prefix.len() <= 256
            && manifest.document_prefix.len() <= 256
            && !manifest.query_prefix.chars().any(char::is_control)
            && !manifest.document_prefix.chars().any(char::is_control),
        "model query/document prefixes must be control-free and at most 256 bytes"
    );
    anyhow::ensure!(
        is_hex(&manifest.revision, 40),
        "model manifest revision must be a full commit hash"
    );
    anyhow::ensure!(
        valid_repository(&manifest.repository),
        "model manifest repository must be an owner/repository name"
    );
    let mut total_size = 0u64;
    for file in &manifest.files {
        anyhow::ensure!(
            valid_remote_path(&file.remote),
            "invalid remote model manifest path: {}",
            file.remote
        );
        anyhow::ensure!(
            is_safe_basename(&file.name),
            "invalid local model manifest path: {}",
            file.name
        );
        anyhow::ensure!(
            file.size > 0,
            "model file size must be positive: {}",
            file.name
        );
        anyhow::ensure!(
            file.size <= 512 * 1024 * 1024,
            "model file exceeds the 512 MiB download limit: {}",
            file.name
        );
        total_size = total_size
            .checked_add(file.size)
            .ok_or_else(|| anyhow::anyhow!("model manifest total size overflow"))?;
        anyhow::ensure!(
            is_hex(&file.sha256, 64),
            "model file SHA-256 must be 64 hexadecimal characters: {}",
            file.name
        );
    }
    anyhow::ensure!(
        total_size <= 1024 * 1024 * 1024,
        "model manifest exceeds the 1 GiB aggregate download limit"
    );
    Ok(())
}

pub fn configured_dimension() -> Result<usize> {
    Ok(expected_manifest(&crate::embedding_runtime::default_model_dir())?.dimension)
}

fn is_hex(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_safe_basename(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && Path::new(value)
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn valid_remote_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && value.split('/').all(|component| {
            !component.is_empty()
                && component != "."
                && component != ".."
                && component
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
        })
}

fn valid_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    let safe_part = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    };
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repo), None) if safe_part(owner) && safe_part(repo))
}

pub fn verify_dir(dir: &Path) -> Result<ModelManifest> {
    let skip_hash = validate_verification_config()?;
    let manifest = expected_manifest(dir)?;
    for file in &manifest.files {
        let path = dir.join(&file.name);
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("model file missing: {}", path.display()))?;
        anyhow::ensure!(
            metadata.is_file() && metadata.len() > 0,
            "model file is empty or not regular: {}",
            path.display()
        );
        if skip_hash {
            continue;
        }
        anyhow::ensure!(
            metadata.len() == file.size,
            "model file size mismatch for {}: expected {}, got {}",
            file.name,
            file.size,
            metadata.len()
        );
        let actual = sha256_file(&path)?;
        anyhow::ensure!(
            actual == file.sha256,
            "model SHA-256 mismatch for {}: expected {}, got {}",
            file.name,
            file.sha256,
            actual
        );
    }
    Ok(manifest)
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}
