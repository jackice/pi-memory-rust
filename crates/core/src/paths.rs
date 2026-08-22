//! Database path resolution: isolated by projectScope (matches the TS paths.ts).
//!
//! Layout: <data_dir>/memories/<base64url(projectScope)>/memory.db
//! data_dir defaults to ~/.local/share/pi-memory-rust (override with PI_MEMORY_DATA_DIR).

use std::path::PathBuf;

/// Default data root (base64url encoding like the TS normalizeProjectScope).
pub fn default_data_dir() -> PathBuf {
    if let Ok(path) = std::env::var("PI_MEMORY_DATA_DIR") {
        return PathBuf::from(path);
    }
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|home| PathBuf::from(home).join(".local").join("share"))
                .unwrap_or_else(|_| std::env::temp_dir())
        });
    base.join("pi-memory-rust")
}

/// projectScope → base64url encoding (no padding), matching the TS version.
pub fn normalize_project_scope(project_scope: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(project_scope.as_bytes());
    if encoded.is_empty() {
        "root".to_string()
    } else {
        encoded
    }
}

/// Database file path for a project.
pub fn resolve_project_db_path(data_dir: &std::path::Path, project_scope: &str) -> PathBuf {
    data_dir
        .join("memories")
        .join(normalize_project_scope(project_scope))
        .join("memory.db")
}

/// Ensure the project database's parent directory exists.
pub fn ensure_project_db_dir(data_dir: &std::path::Path, project_scope: &str) -> std::io::Result<PathBuf> {
    let path = resolve_project_db_path(data_dir, project_scope);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_encoding_matches_ts_base64url() {
        // base64url result of the TS normalizeProjectScope("/proj/demo")
        assert_eq!(normalize_project_scope("/proj/demo"), "L3Byb2ovZGVtbw");
        assert_eq!(normalize_project_scope(""), "root");
    }

    #[test]
    fn different_scopes_resolve_to_different_paths() {
        let dir = std::path::Path::new("/tmp/pi-memory-data");
        let a = resolve_project_db_path(dir, "/proj/a");
        let b = resolve_project_db_path(dir, "/proj/b");
        assert_ne!(a, b);
        assert!(a.to_string_lossy().ends_with("memory.db"));
    }
}

