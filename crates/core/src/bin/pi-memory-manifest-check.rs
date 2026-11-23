use anyhow::{Context, Result};
use std::path::PathBuf;

fn main() -> Result<()> {
    let paths = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    anyhow::ensure!(!paths.is_empty(), "pass one or more model manifest paths");

    for path in paths {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read manifest {}", path.display()))?;
        let manifest = pi_memory_core::model_manifest::parse_manifest_json(&raw)
            .with_context(|| format!("validate manifest {}", path.display()))?;
        println!(
            "{}: {} ({} files, {} dimensions)",
            path.display(),
            manifest.model_id,
            manifest.files.len(),
            manifest.dimension
        );
    }
    Ok(())
}
