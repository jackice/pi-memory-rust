//! M1 起步用的内存存储：验证协议与桥接链路，不依赖 turso_core 具体 API。
//! T2 将替换为 turso_core 文件存储实现（保留 Storage trait 不变）。

use super::Storage;

/// 内存存储：仅用于协议/桥接验证。
#[derive(Default)]
pub struct MemStorage;

impl Storage for MemStorage {
    fn init(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn diagnose(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "backend": "mem",
            "status": "ok",
        }))
    }
}
