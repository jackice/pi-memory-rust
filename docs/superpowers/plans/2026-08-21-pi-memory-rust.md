# pi-memory-rust 实施计划

- 作者：jackice
- 日期：2026-08-21
- 关联设计：`docs/superpowers/specs/2026-08-21-pi-memory-rust-turbovec-design.md`（v4）
- 状态：进行中（M1）

---

## 1. 目标与范围

在独立新仓库 `pi-memory-rust` 中用 Rust 重写 pi-memory（turso_core 存储 + Tantivy FTS + turbovec 向量 + ort embedding），pi extensions 侧通过 NAPI 二进制集成（参照 oh-my-pi 模式）。现有 pi-memory（TS v0.1.2）保持不动，作为行为基准。

**关键依赖版本**（已验证可用）：
- `turbovec = "1.0.0"`（crates.io）
- `turso_core = "0.7.2"`（crates.io；features 裁剪为 `fs + json + fts`）
- `ort = "2.0.0"`（crates.io，`download-binaries`）
- Rust 工具链 ≥ 1.89（本机 1.97.1）

**桥接方式（v5 决策）**：子进程 JSON-RPC（stdio，常驻），非 NAPI。理由见 spec D1：崩溃隔离、超时可强杀、工程最轻、与宿主版本无关；低频调用下 NAPI 性能优势无意义。

## 2. 分期

| 阶段 | 内容 | 验收 |
|---|---|---|
| M1 | 工程骨架 + Rust 核心替换（行为等价） | 现有 bun test 规则路径用例在 Rust 侧等价通过；/memory 命令可用；NAPI 加载链路通 |
| M2 | ort embedding + turbovec 索引 + 混合召回 | 召回质量优于 local-hash；延迟达标 |
| M3 | LLM 代理 + 迁移工具 + 多平台构建 | 全功能可用 |

---

## 3. M1 任务分解

### T1 工程骨架（仓库级）

1. Cargo workspace（`crates/core` 含 lib + bin）`+ rust-toolchain.toml`（锁定 1.89+）。
2. `crates/core`：纯库（lib.rs）承载全部业务逻辑 + `protocol.rs`（JSON-RPC 请求/响应类型，带版本握手） + `src/bin/pi-memory-server.rs`（stdio JSON-RPC 常驻服务）。
3. TS 侧：`ts/index.ts`（pi 扩展入口：事件绑定 + /memory 命令 + LLM 代理接口）、`ts/src/client.ts`（JSON-RPC 客户端 + 进程生命周期：spawn/超时/崩溃重启/诊断，参照 oh-my-pi loader 隔离思路）、`package.json`（`@jackice/pi-memory-rust`，`pi.extensions` 指向 index.ts）。
4. `scripts/`：构建、leaf 包生成（预留）。
5. 验收：`cargo check` 通过；bin 可运行且协议握手验证通过；TS 客户端可拉起常驻进程并完成一次调用。

### T2 存储层移植（turso_core）

移植 `src/storage/sqlite/`（schema.ts / store.ts / session-state.ts / paths.ts）：

1. `schema.ts` 表结构 → Rust 建表 SQL（working_memory / episodic_memory / facts / preferences / instructions / timeline / memory_embeddings / session_state；FTS 改为 Turso Tantivy 语法 `CREATE INDEX ... USING fts`）。
2. `SqliteMemoryStore` CRUD 全量移植 → `turso_core` 连接封装（`Storage` trait 隔离，保留将来切回 rusqlite 的可能）。
3. 现有 SQLite 库读取验证（Guarantee 2：文件格式兼容）。
4. 验收：`test/storage/{schema,store}.test.ts` 对应用例移植通过。

### T3 规则路径移植（行为等价）

移植 `src/core/` 与 `src/app/` 的纯规则路径（不接 LLM）：

1. `recall.ts` 多路合并排序 → Rust（**bit-exact**：同累加顺序、同 tie-breaking、同权重公式 `fts*1.5 + embedding + structured + scopeBoost`）。
2. `extraction.ts` 规则提取（extractWithHeuristics / inferFact / inferPreference / inferInstruction / inferTimeline / parseFactStatement）→ Rust。
3. `consolidation.ts` 规则降级摘要（有界 fallback）→ Rust。
4. `runtime.ts` 编排（retain cadence / recall 注入 / session 整合）→ Rust 核心 API。
5. 验收：`test/core/*.test.ts` 与 `test/app/*.test.ts` 规则路径用例移植通过；`/memory view|stats|diagnose|clear|rebuild` 经 NAPI 可用。

### T4 桥接与命令

1. napi 层导出：`retainTurnMemories` / `buildRecallInjection` / `consolidateSessionMemories` / `diagnose` / `clear` / `rebuild`（typed array 用于 embedding 批量传入）。
2. TS 薄壳：绑定 pi 事件（before_agent_start / turn_end / session_shutdown）+ 注册命令。
3. 全链路超时与失败降级（二进制/引擎异常不阻塞 pi 主流程）。
4. 验收：pi 侧 `/memory` 命令全通；行为与 TS 版一致。

---

## 4. M2 任务分解（概要）

1. ort + all-MiniLM-L6-v2：模型加载（延迟 + 缓存目录 + 失败降级，参照 mnemopi）、embed 批处理（Float32Array 跨层）。
2. turbovec `IdMapIndex<dim=384, bit_width=4>`：add/remove/allowlist search；索引载荷持久化（DB BLOB 经 `from_parts` 或独立文件 + `sync`）。
3. 混合召回：Tantivy FTS 粗筛 → id allowlist → turbovec 精排 → 并入 recall 加权。
4. 验收：`PI_MEMORY_EMBEDDING_PROVIDER` 语义切换；召回质量抽样对比优于 local-hash；万级延迟 ≤ 300ms。

## 5. M3 任务分解（概要）

1. LLM 代理：Rust 发起请求 → TS 薄壳调 pi provider → 回传（提取/整合模板在 Rust 侧）。
2. 迁移工具：现有 TS 版 SQLite 库 → 新库（数据 + FTS 重建）。
3. 多平台 leaf 包构建 + 发布脚本。
4. `/memory diagnose` 完善（provider 状态矩阵）。

---

## 6. 验收标准（总）

1. 行为等价：M1 后现有 bun test 规则路径用例在 Rust 侧通过。
2. FTS 迁移：Tantivy FTS 重建后不漏召回（允许 BM25 排序差异）。
3. 召回质量：本地语义 embedding 显著优于 local-hash。
4. 性能：万级记忆 recall 注入 ≤ 300ms。
5. 降级：二进制/模型/LLM 缺失均不阻塞 pi 主流程。
6. 数据兼容：现有 SQLite 库可直接迁移，无数据丢失。

---

## 7. 风险与对策

| 风险 | 对策 |
|---|---|
| turso_core beta / API 变动 | 锁版本 0.7.2；Storage trait 隔离；SQLite 兼容可回退 |
| FTS5 → Tantivy 语法迁移 | M1 以不漏召回为门槛 |
| turso_core 异步引擎桥接 | 已规避：Rust 侧自管 tokio runtime（子进程方案无桥接层） |
| 行为差异（bit-exact） | 移植测试用例时逐位对照 |
| onnxruntime 加载健壮性 | 延迟加载 + 缓存目录 + 降级（参照 mnemopi） |
