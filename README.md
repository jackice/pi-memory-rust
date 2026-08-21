# pi-memory-rust

Rust 重写版 pi-memory：Pi 的本地记忆扩展，存储与检索全部在 Rust 常驻子进程内完成。

## 架构

```
pi 宿主 (Node ≥ 22.19 / Bun)
└── TS 薄壳 (ts/)                事件绑定 + /memory 命令 + LLM 代理
        │  JSON-RPC over stdio（常驻进程，崩溃隔离 + 超时强杀）
        ▼
    Rust 子进程 (crates/core)
    ├── turso_core               存储引擎（SQLite 兼容格式 + Tantivy FTS）
    ├── turbovec                 向量 ANN（量化 + SIMD + allowlist 精排）[M2]
    └── ort                      ONNX embedding（all-MiniLM 384 维）[M2]
```

设计决策（详见 `docs/superpowers/specs/`）：

- **子进程 JSON-RPC 而非 NAPI**：崩溃隔离（Rust 崩溃不影响 pi 宿主）、超时可强杀、工程最轻、与宿主版本无关；低频调用下 NAPI 的性能优势无意义。
- **重活在子进程内完成**：只交换小负载（prompt、Top-K 候选），embedding 批量不外传。
- **行为等价**：recall/retain/consolidate 与 TS 参考实现（pi-memory v0.1.2）逐位一致。

## 开发

```bash
# 构建 Rust 服务
cargo build --release
cp target/release/pi-memory-server ts/bin/

# Rust 测试
cargo test

# TS 冒烟测试（常驻进程生命周期 + JSON-RPC 链路）
bun test ts/test/
```

## 状态

- [x] M1-T1 工程骨架：cargo workspace、JSON-RPC 协议（版本握手）、TS 客户端（spawn/超时/崩溃重启）
- [x] M1-T2 存储层移植（turso_core + Tantivy FTS）
- [x] M1-T3 规则路径移植（recall / extraction / consolidation，行为等价）
- [x] M1-T4 桥接与 /memory 命令
- [x] M2 语义 embedding + turbovec 索引（混合召回，FTS 盲区命中 0.01s）
- [x] M3 LLM 代理（llmResult 单向 RPC）+ 迁移工具（migrate）+ 多平台 leaf 包

## 功能入口

| 入口 | 说明 |
|---|---|
| `/memory view\|stats\|diagnose\|clear\|rebuild` | 记忆维护 |
| `/memory migrate <旧库路径>` | 从 TS 版 pi-memory SQLite 库迁移 |
| `/memory prepare-model` | 手动触发 embedding 模型下载 |
| `PI_MEMORY_MODEL_PATH` | 指定模型目录（缺省 `~/.cache/pi-memory-rust/model`） |
| `PI_MEMORY_RUST_BINARY` | 指定 Rust 二进制路径 |
| `PI_MEMORY_DB` / `--db` | 指定数据库文件路径 |

## 模型

all-MiniLM-L6-v2（384 维，~90MB ONNX）：首次启动后台线程自动下载（HF 官方 + hf-mirror 镜像回退），缺失时自动降级 FTS-only；手动触发见 `/memory prepare-model`。
