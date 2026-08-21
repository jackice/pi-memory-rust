# @jackice/pi-memory-rust

Pi 的 Rust 记忆扩展：Turso 存储 + Tantivy FTS + turbovec 向量 + 本地 ONNX embedding。

完整文档见仓库根目录 `README.md`（本包为 npm 发布用入口）。

## 结构

- `index.ts` — pi 扩展入口（事件绑定 + /memory 命令 + LLM 代理）
- `src/client.ts` — JSON-RPC 客户端 + 常驻进程生命周期管理
- 二进制由 leaf 包提供：`@jackice/pi-memory-rust-<platform>-<arch>`（本包 optionalDependencies）

## 环境变量

| 变量 | 说明 |
|---|---|
| `PI_MEMORY_RUST_BINARY` | 指定 Rust 二进制路径（覆盖 leaf 包/包内 bin） |
| `PI_MEMORY_DATA_DIR` | 数据目录（默认 `~/.local/share/pi-memory-rust`） |
| `PI_MEMORY_MODEL_PATH` | embedding 模型目录（默认 `~/.cache/pi-memory-rust/model`） |
