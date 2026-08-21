//! 模型下载：all-MiniLM-L6-v2（sentence-transformers）自动获取。
//!
//! 策略：官方源 → 镜像（hf-mirror.com）→ 本地路径（PI_MEMORY_MODEL_PATH）。
//! 下载失败不阻塞主流程（embedding 降级 FTS-only）。

use anyhow::{Context, Result};

/// 官方源与镜像基址（模型仓库根目录）
const HF_OFFICIAL: &str = "https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/main";
const HF_MIRROR: &str = "https://hf-mirror.com/sentence-transformers/all-MiniLM-L6-v2/resolve/main";

/// 文件清单：远程相对路径 → 本地文件名
const REMOTE_FILES: [(&str, &str); 2] = [
    ("onnx/model.onnx", "model.onnx"),
    ("tokenizer.json", "tokenizer.json"),
];

/// 检查模型目录是否完整。
pub fn model_dir_complete(dir: &std::path::Path) -> bool {
    dir.join("model.onnx").exists() && dir.join("tokenizer.json").exists()
}

/// 确保模型目录可用：完整则直接返回；否则下载缺失文件。
/// 下载全部失败时返回 Err（调用方降级）。
pub fn ensure_model_dir(dir: &std::path::Path) -> Result<()> {
    if model_dir_complete(dir) {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("创建模型目录失败: {}", dir.display()))?;

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
            "模型下载不完整（{}）。可手动放置 model.onnx/tokenizer.json 到 {}，或用 PI_MEMORY_MODEL_PATH 指定目录",
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
                eprintln!("[pi-memory-rust] 模型文件下载完成: {url}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("[pi-memory-rust] 下载失败（尝试下一源） {url}: {e}");
            }
        }
    }
    anyhow::bail!("所有源均失败")
}

fn download_file(url: &str, target: &std::path::Path) -> Result<()> {
    let response = ureq::get(url).call().with_context(|| format!("HTTP 请求失败: {url}"))?;
    let mut body = Vec::new();
    let mut reader = std::io::Read::take(response.into_body().into_reader(), 512 * 1024 * 1024); // 512MB 上限
    std::io::Read::read_to_end(&mut reader, &mut body).with_context(|| "读取响应失败")?;
    let tmp = target.with_extension("part");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, target)?;
    Ok(())
}
