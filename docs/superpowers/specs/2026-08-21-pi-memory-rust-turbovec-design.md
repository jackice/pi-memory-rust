# pi-memory Rust 重写 + turbovec 集成设计文档

- 作者：jackice
- 日期：2026-08-21（v3：项目组织决策更新为独立新仓库）
- 状态：待确认（draft）
- 关联仓库：[pi-memory](https://github.com/jackice/pi-memory)（当前）、[turbovec](https://github.com/RyanCodrai/turbovec)（向量索引）、[turso](https://github.com/tursodatabase/turso)（存储引擎）

---

## 1. 背景与目标

### 1.1 背景

pi-memory 是 Pi 的 SQLite-first 记忆扩展，当前为纯 TypeScript 实现（约 3,170 行含测试）。用户希望将 [turbovec](https://github.com/RyanCodrai/turbovec)（基于 Google TurboQuant 的 Rust 向量索引）的能力集成到 RAG 召回链路，并**用 Rust 完全重写 pi-memory**，在 pi extensions 中直接集成二进制。

### 1.2 已确认的目标（来自需求澄清）

| 维度 | 决策 |
|---|---|
| 核心目标 | 召回质量与检索性能**两者都要** |
| 技术路线 | Rust 完全重写 + pi extensions 集成二进制 |
| embedding | 本地 ONNX 模型（如 all-MiniLM，384 维） |
| 存储引擎 | **Turso database（`tursodatabase/turso`）底层引擎库**轻量嵌入，不要整套服务 |
| 多机同步 | 暂不需要 |
| 架构诉求 | 高性能 + 架构简洁 |
| 项目组织 | **独立新仓库 `pi-memory-rust`，现有 pi-memory 仓库原地不动** |

### 1.3 项目组织决策（v3）

**决策**：新建独立仓库 `pi-memory-rust`（如 `github.com/jackice/pi-memory-rust`，本地开发目录与 pi-memory 同级），基于现有 pi-memory 的内容（代码、测试、设计）开发；**现有 pi-memory 仓库保持不动**，v0.1.2 继续可用与维护。

**理由**：

1. 这是**全栈重写**（TS → Rust、SQLite → Turso 引擎、无 ANN → turbovec、hash embedding → ONNX），不适合原地演进：中途状态不可用、git 历史混乱、无法回退。
2. 现有 TS 版继续作为**可用版本与行为基准**：bug 修复不受影响，两个版本可并行对照。
3. 设计文档随新仓库迁移（本 spec 将迁移至新仓库 `docs/superpowers/specs/`，原仓库保留副本供追溯）。

**新仓库结构（规划）**：

```
pi-memory-rust/
├── Cargo.toml                 # workspace（版本 / lint 配置 workspace 共享）
├── rust-toolchain.toml        # 锁定工具链（≥1.89，turbovec 硬要求）
├── crates/core/               # 核心逻辑（纯 Rust）：
│   ├── src/lib.rs             #   turso_core 存储 + Tantivy FTS + turbovec 索引 + ort embedding + recall/retain/consolidate
│   ├── src/protocol.rs        #   JSON-RPC 协议类型（请求/响应/版本握手）
│   └── src/bin/pi-memory-server.rs  # stdio JSON-RPC 服务入口（常驻进程）
├── ts/                        # TS 侧
│   ├── index.ts               # pi 扩展入口（事件绑定 + /memory 命令 + LLM 代理）
│   ├── src/client.ts          # JSON-RPC 客户端 + 常驻进程生命周期管理（spawn/超时/崩溃重启/诊断）
│   └── package.json           # @jackice/pi-memory-rust，pi.extensions 指向 index.ts；optionalDependencies 声明 leaf 包
├── leaf-packages/             # 按平台 leaf 包（@jackice/pi-memory-rust-<tag>），脚本生成（参照 oh-my-pi gen-npm-packages.ts）
├── tests/                     # Rust 集成测试（行为基准来自现有 bun test 用例移植；关键数值逻辑保持 bit-exact 对照）
├── scripts/                   # 构建 / leaf 包生成 / 发布
└── docs/superpowers/          # spec/plan 文档（本设计文档迁移至此）
```

**工程化要点（结合 oh-my-pi 模式）**：无需 Bazel 与 napi-rs——子进程方案下 Rust 是普通 bin；协议带版本握手（替代 NAPI 版本哨兵）；每平台 leaf 包 optionalDependencies 分发；加载器/客户端与业务隔离（可单测）；turso_core 异步引擎由 Rust 侧自管 tokio runtime（无桥接层集成问题）。

**测试基准策略**：现有 pi-memory 的 `bun test` 用例作为**行为等价基准**，逐类移植为 Rust 集成测试（retain 持久化、FTS 召回、recall 合并排序、consolidate 降级等）；M1 验收以「Rust 侧通过移植用例」为准。

### 1.4 现有痛点

1. **embedding 质量低**：默认 `local-hash` 是 3 维布尔特征向量（`runtime.ts:486-491`），无真实语义；未配置时完全退化为 FTS。
2. **无 ANN 索引**：`buildEmbeddingCandidates`（`runtime.ts:419-463`）对每条记忆全量线性扫描 + 逐条读 SQLite 算余弦，无任何近似最近邻结构，记忆量级增大后召回延迟线性恶化。
3. **双运行时脆弱**：bun:sqlite / node:sqlite 双驱动（Node < 22.13 无内置 sqlite），宿主差异带来的兼容面。
4. **检索与排序割裂**：embedding 候选、FTS 候选、结构化候选是三条独立路径，最后在 TS 侧合并（`recall.ts` 加权 `fts*1.5 + embedding + structured + scopeBoost`），无法利用向量索引的 allowlist 过滤在 SIMD 内核内做混合精排。

---

## 2. 现状与依赖调研

### 2.1 pi-memory 现有架构（TS）

```
src/adapter/pi/index.ts         扩展入口：绑定 pi 事件 + /memory 命令
  ├─ before_agent_start → buildRecallInjection（召回注入）
  ├─ turn_end → retainTurnMemories（节奏 retain）
  ├─ session_shutdown → retain + consolidateSessionMemories
  └─ commands.ts                 /memory view|stats|diagnose|clear|rebuild
src/app/runtime.ts              编排层：recall / retain / consolidate、embedding provider 解析、超时降级
src/core/
  ├─ recall.ts                  多路候选合并 + 加权排序
  ├─ extraction.ts              结构化提取（LLM 优先，规则降级）
  └─ consolidation.ts            会话整合（LLM，失败降级为有界摘要）
src/storage/sqlite/
  ├─ schema.ts                  memory_embeddings / FTS 等表结构
  ├─ driver.ts                  bun:sqlite ↔ node:sqlite 双驱动
  ├─ store.ts                   CRUD + embedding 存取
  └─ session-state.ts           会话状态
src/providers/                  EmbeddingProvider / LlmProvider 接口
```

数据流：

- **retain**：turn 文本 →（可选 LLM 结构化提取）→ 写入 working/episodic + FTS →（可选）embed 存入 `memory_embeddings`。
- **recall**：prompt → FTS 候选 + embedding 候选（全扫描）+ 结构化候选 → `recall.ts` 合并加权排序 → 注入系统提示。
- **consolidate**：session 结束 → LLM 整合为 episodic 记忆。

### 2.2 pi extensions 机制（关键约束）

- 扩展入口**必须是 TS/JS 模块**，经 jiti 加载（`~/.pi/agent/extensions/*.ts` 或 package.json `pi.extensions` 字段声明）。
- 事件、工具、命令、UI 全部是 JS API；**LLM 调用（`ctx.modelRegistry` / provider）只能在 TS 侧发起**。
- 宿主为 Node ≥ 22.19（`@earendil-works/pi-coding-agent` engines 声明）；Node 与 Bun 均可加载 `.node` 原生模块。
- 扩展工厂不应在非会话上下文启动后台资源；长驻资源延迟到 `session_start`，在 `session_shutdown` 清理。

**推论**：即使核心逻辑全部用 Rust 实现，pi 侧仍需要一个 TS 薄壳负责事件绑定、命令注册、LLM 代理与 UI 通知。Rust 无法直接注册 pi 事件，这是架构的上界。

### 2.3 turbovec 能力（向量索引集成对象）

Rust crate `turbovec = "1.0.0"`，MIT license，rust-version 1.89，依赖轻（rayon / rand / statrs 等，纯 Rust）。

| 能力 | 说明 | 对本项目的价值 |
|---|---|---|
| TurboQuant 量化 | 2/3/4-bit 压缩，10M 文档 float32 31GB → 4GB | 内存占用降低 ~8× |
| SIMD 搜索 | AVX-512 / NEON 手写内核，优于 FAISS IndexPQFastScan | 召回延迟显著下降 |
| 在线增量摄入 | add 即索引，无训练、无重建 | retain 无需批处理 |
| `IdMapIndex` | 稳定 u64 外部 id + O(1) 删除 | 记忆 id 稳定映射 |
| 搜索 allowlist | SIMD 内核内过滤候选集合 | **契合 FTS → 向量精排的混合召回** |
| `from_parts` | 从内存字段直接构造索引 | 可把索引载荷存 DB BLOB，绕开文件 |
| `sync(path)` | 增量持久化，单次 fsync | 低写入放大 |

### 2.4 Turso database 引擎调研（存储层集成对象）

Turso（`tursodatabase/turso`，23.9k stars）是 SQLite 的 **Rust 全新重写**（非 fork），其核心引擎以 **`turso_core` crate** 形式独立发布（"The Turso database library"），可从 Rust 直接轻量嵌入，**无需 sqld 服务 / Turso Cloud**。关键事实（来源：官方 COMPAT.md，追踪 SQLite 3.50.4）：

| 事实 | 状态 | 对 pi-memory 的影响 |
|---|---|---|
| SQLite 文件格式 | ✅ 完全支持；官方 Guarantee："随时可回退 SQLite"、"可直接读 SQLite 创建的库" | 数据迁移路径清晰；SQLite 始终是逃生舱 |
| FTS | ⚠️ **用 Tantivy 实现，不支持 SQLite FTS3/4/5 语法**（`CREATE INDEX ... USING fts` + `fts_match()`/`fts_score()` BM25） | 现有 FTS5 召回**必须重写为 Tantivy 语法**；Tantivy 是成熟全文引擎，BM25 质量不弱于 FTS5 |
| 原生向量 | ⚠️ SQL 函数（`vector_distance_cos` 等）可用，但索引实现为 **`toy_vector_sparse_ivf`（实验性玩具实现）** | **高性能 ANN 仍必须依赖 turbovec**，不能省 |
| 查询语言/C API | 🚧 部分支持，差异经 differential testing 持续修复 | 需避免冷门 SQL 特性；普通 CRUD/JSON 可用 |
| UTF-8 限制 | 文本必须是合法 UTF-8，非 UTF-8 字节转文本时替换为 U+FFFD | pi-memory 数据均为 UTF-8 文本，无实际影响 |
| 多进程 | 不支持混合 SQLite/Turso 多进程访问 | pi-memory 单进程，无碍 |
| 状态 | **beta**，API 可能变动 | 锁定版本 + 隔离在存储 trait 之后 |
| 依赖裁剪 | features：`default = [fs, uuid, time, json, series, encryption, percentile]`，`fts` 独立 feature（tantivy） | 嵌入式可按需裁剪：`fs + json + fts`（可去掉 encryption/percentile 等） |

### 2.5 Rust embedding 生态

| 方案 | 说明 | 取舍 |
|---|---|---|
| `ort` crate | ONNX Runtime 绑定，成熟、CPU/GPU 支持好 | 依赖 onnxruntime 动态库（可 bundled） |
| `candle` | HuggingFace 纯 Rust 推理，无外部运行时 | 更轻，但算子覆盖与性能弱于 ORT |

模型选择：`all-MiniLM-L6-v2`（384 维，~90MB ONNX）作为默认；模型文件首次运行下载或随包分发。

### 2.6 oh-my-pi TS+Rust 集成模式调研（最佳实践参考）

调研对象：`oh-my-pi`（本地仓库，`crates/pi-natives` + `packages/natives` + `packages/mnemopi`）。oh-my-pi 是 ~80k 行 Rust 核心 + TS 的 Pi fork，其 TS+Rust 集成为生产级成熟范式。**重点：`@oh-my-pi/pi-mnemopi` 是其本地 SQLite 记忆引擎，与 pi-memory 目标高度重合**（fastembed + onnxruntime-node + Rust natives 向量内核）。

**集成模式（crates/pi-natives ↔ packages/natives）**：

| 层 | 做法 | 可复用点 |
|---|---|---|
| Rust crate | `crate-type = ["cdylib"]` + napi-rs（`napi`/`napi-derive`）；能力模块化拆分 crate（pi-ast/pi-shell/pi-walker…），napi 层聚合导出 | 我们：`crates/core`（业务）+ `crates/napi`（绑定）同构 |
| 数值传参 | **typed array 零拷贝**（`Float32Array`/`Float64Array`/`Uint32Array`）一次跨层传整个批量；矩阵 row-major 展平 + dim 参数 | embedding/recall 大批量处理的性能关键 |
| 异步引擎 | napi-rs 内置 tokio 集成：`create_custom_tokio_runtime`（Windows）或默认 runtime + `__ompInstallTokioRuntime` 显式安装；blocking 重活在 `spawn_blocking`/rayon | **turso_core 异步引擎桥接的现成答案** |
| 行为等价 | **bit-exact 契约**：Rust 内核与 TS 参考实现同累加顺序、同非有限数处理、同 tie-breaking，浮点分数逐位一致 | 我们移植 bun test 用例做行为等价的验收方式 |
| 版本 | **版本哨兵**：`.node` 导出 `__piNativesV{version}`，JS 加载器校验与 package.json 一致，防新旧不匹配 | 二进制分发一致性保障 |
| JS 加载器 | `loader-state.js` 独立模块：平台/变体检测、候选路径解析、embedded-addon 提取（Bun standalone）、版本校验、聚合错误面；`index.js` 仅 `loadNative()` + 生成 exports；`index.d.ts` 由 napi-rs 生成 | 加载逻辑与业务隔离、可单测 |
| 平台分发 | **leaf 包**：`@oh-my-pi/pi-natives-<tag>`（linux-x64/linux-arm64/darwin-x64/darwin-arm64/win32-x64），`scripts/gen-npm-packages.ts` 生成，父包 optionalDependencies；x64 还分 modern/baseline（AVX2 检测） | 我们：`@jackice/pi-memory-rust-<tag>` leaf 包 |
| 构建 | Bazel + `bazel-natives.ts` host 构建、`embed-native.ts` 嵌入；本仓库可简化为 napi-rs 官方 CLI（`napi build`） | 小项目不需要 Bazel |

**mnemopi 的关键实践（记忆引擎同源参考）**：

- **Rust 只做热路径数值内核**（`vectorIndexTopK` 精确 top-k、`cosineSimilarityPairs`），业务编排留在 TS；
- embedding 用 **fastembed + onnxruntime-node**（JS 生态 ONNX，peerDependencies 声明），模型缓存目录 + 加载失败降级处理（含 onnxruntime 已知 segfault 规避）；
- Float32Array 紧凑矩阵形态，「为未来 ANN/量化后端预留形状」——与我们引入 turbovec 的方向一致；
- 其向量检索为**精确 top-k**（Rust 加速全量打分）而非 ANN；我们选 turbovec 量化 ANN 是为了更大规模 + 内存压缩，路线更激进但用户已确认。

**对我们的启示（取舍）**：

1. oh-my-pi 是「TS 业务 + Rust 热路径内核」；用户已确认「完全 Rust 重写 + 二进制集成」。工程模式（行为等价测试、leaf 包分发、加载器隔离、崩溃诊断）复用；**桥接方式不照搬 NAPI**（oh-my-pi 的 natives 是高频热路径基础设施，pi-memory 是低频调用，场景不同，见 D1）。
2. embedding 模型加载的健壮性经验（延迟加载、缓存目录、失败降级）必须纳入（对应我们 D3）。
3. 我们无需 Bazel 与 napi-rs CLI：子进程方案下 Rust 是普通 bin，简单脚本即可满足构建与 leaf 包生成。

---

## 3. 目标架构

### 3.1 总体分层

```
┌─────────────────────────────────────────────────────────┐
│  pi 宿主（Node ≥ 22.19 / Bun）                            │
│  ┌───────────────────────────────────────────────────┐  │
│  │ TS 薄壳（adapter，必留）                            │  │
│  │  · pi 事件绑定（before_agent_start/turn_end/…）    │  │
│  │  · /memory 命令注册                                 │  │
│  │  · LLM 代理（Rust 请求 → 这里调 pi 的 provider）    │  │
│  │  · UI 通知 / 降级提示                               │  │
│  └───────────────┬───────────────────────────────────┘  │
└───────────────────┼──────────────────────────────────────┘
                    │ 桥接（D1 决策：子进程 JSON-RPC）
┌───────────────────▼──────────────────────────────────────┐
│ Rust 核心（常驻子进程）                                    │
│  · turso_core 存储引擎（SQLite 兼容格式 + Tantivy FTS）   │
│  · turbovec 向量索引（IdMapIndex + allowlist 精排）       │
│  · ONNX embedding（ort + all-MiniLM）                     │
│  · recall 多路合并 / retain / consolidate（规则版）        │
│  · 规则降级提取（无 LLM 时）                              │
│  · stdio JSON-RPC 服务（重活在子进程内完成，只回传小结果）│
└──────────────────────────────────────────────────────────┘
```

**组件关系（TS 宿主 + Rust 常驻子进程）**：

- `turso_core`：持久化（SQLite 兼容文件格式）+ 会话状态 + **Tantivy FTS**（替代 SQLite FTS5）。
- `turbovec`：embedding 向量 ANN（量化 + SIMD + allowlist），弥补 Turso 原生向量索引不成熟的问题。
- embedding 向量本身仍存 DB（`memory_embeddings` 表），turbovec 索引载荷经 `from_parts` 从 DB BLOB 重建，或独立索引文件 + `sync` 增量写。
- **跨进程数据面**：请求/响应均为小负载（prompt、Top-K 候选、配置），embedding 批量等重活在子进程内完成，不外传大向量。

### 3.2 关键决策点

#### D1 桥接方式（TS ↔ Rust）——v5 更新：子进程 JSON-RPC

| 方案 | 优点 | 代价 | 结论 |
|---|---|---|---|
| **A. 子进程 JSON-RPC（stdio，常驻）** | 崩溃隔离（Rust 崩溃不影响 pi 宿主）；超时可强杀；工程最轻（普通 bin + 协议）；ABI 与宿主版本无关；LLM 代理天然；异步引擎完全自控 | 常驻进程生命周期管理；JSON 序列化（重活内置后负载小，可忽略） | **推荐** |
| B. NAPI（napi-rs） | 内存共享、零序列化、延迟最低 | 需预编译 `.node`；ABI 绑定宿主；**Rust 崩溃 = pi 宿主崩溃**；工具链重；LLM 代理需 threadsafe 回调 | 远期选项 |
| C. 本地 daemon（HTTP/Unix socket） | 完全解耦、可多进程共享索引 | 生命周期管理复杂，明显过重 | 不推荐 |

**推荐 D1=A（v5 决策）**：低频调用场景下 NAPI 的性能优势无意义，其工程复杂度与崩溃风险（turso_core beta + onnxruntime + SIMD 内核的高风险组合）是实打实成本。子进程方案以「常驻进程 + 子进程内做重活 + 只回传小结果」消除性能顾虑，以「进程崩溃隔离 + 超时 kill + 自动重启」架构性满足降级目标。核心逻辑在 `crates/core`，将来确有高频热路径需求时可在其上叠加 NAPI 绑定，无需重构核心。

**生命周期管理**：TS 侧在 `session_start` spawn 常驻进程、`session_shutdown` kill；调用带超时；进程崩溃自动重启（有限次），重启仍失败则降级为 FTS-only 或报错提示。协议带版本握手，支持增量演进。

#### D2 LLM 调用归属（提取/整合）

- **约束**：pi 的 LLM 能力只在 TS 侧。
- **方案**：Rust 侧负责存储、检索、embedding 与规则降级；需要 LLM 时通过桥接发起「LLM 请求」→ TS 薄壳调用 pi provider（`ctx.modelRegistry`）→ 回传结果。提取/整合 prompt 模板放在 Rust 侧，TS 仅做无状态转发。
- **降级**：LLM 不可用/超时 → Rust 规则提取 + 有界摘要（保持现有语义，`extraction.ts` / `consolidation.ts` 的规则逻辑移植）。

#### D3 embedding 模型

- **推荐**：`ort` crate（bundled onnxruntime）+ `all-MiniLM-L6-v2.onnx`（384 维）。
- 模型分发：优先「随包 optional 下载」，`PI_MEMORY_MODEL_PATH` 可覆盖；首用前需 embed 时再加载，避免拖慢 pi 启动。
- 降级：模型缺失 → 回退 FTS + 结构化召回（不阻塞主流程）。

#### D4 向量索引

- **推荐**：依赖 `turbovec` crate，`IdMapIndex<dim=384, bit_width=4>`。**Turso 原生向量索引（toy_vector_sparse_ivf）不成熟，不采用**；仅其 SQL 函数可在诊断/调试中参考。
- 混合召回落地方式（对齐 turbovec 的 allowlist 设计）：
  1. **Tantivy FTS** 粗筛（Turso `fts` feature）→ 候选 id 集合；
  2. 候选 id 作为 allowlist 传给 turbovec `search(query, k, allowlist)`；
  3. 结果并入 `recall` 多路加权（保留现有 `fts*1.5 + embedding + structured + scopeBoost` 公式，权重可在 Rust 侧配置）。
- 持久化：索引载荷经 `from_parts` 存取 DB BLOB（`memory_embeddings` 表扩展/迁移），保持 DB-first；`sync` 增量写作为可选项。

#### D5 存储引擎（v2 更新）

- **推荐**：`turso_core` crate（Turso 底层引擎库）轻量嵌入，features 裁剪为 `fs + json + fts`（去掉 encryption/percentile/series 等）。
- **理由**：用户明确选择 Turso 生态；Rust 原生、无 C 依赖、SQLite 文件格式兼容（随时可回退 SQLite）；单一引擎同时提供持久化 + Tantivy FTS，架构反而更简洁。
- **迁移**：现有 SQLite 库可直接读取（Guarantee 2）；FTS 部分从 SQLite FTS5 语法重建为 Turso Tantivy 语法（`/memory rebuild` 语义保留）。
- **隔离**：存储操作收敛在 `Storage` trait 之后（turso_core 实现），保留将来切回 rusqlite 的可行性，作为 beta 风险的保险。
- **已知风险**：turso_core 为异步引擎（io_uring/线程模型），NAPI 桥接需 block_on / 线程池封装；锁版本号，API 变动由存储 trait 隔离吸收。

#### D6 分发与降级

- **分发**：NAPI `.node` 按平台预编译（macOS arm64/x64、linux-x64、win32-x64），经 pi 包 `optionalDependencies` 按平台安装；无匹配二进制时给出明确诊断信息（`/memory diagnose` 显示 provider 状态）。
- **降级阶梯**：二进制缺失 → 报错 + 指引（v1 不保留 TS 双实现）；模型缺失 → FTS + 结构化；LLM 缺失 → 规则提取。
- **失败隔离**：所有桥接调用带超时（对齐现有 100ms embedding / 有界 consolidate 语义），失败不阻塞 pi 主流程。

---

## 4. 方案对比与推荐

| 方案 | 存储 | FTS | 向量 | 收益 | 风险/成本 |
|---|---|---|---|---|---|
| **A. Turso 引擎 + turbovec（推荐）** | `turso_core` | Tantivy | turbovec | 单一引擎 + 生产级 ANN；SQLite 兼容可回退；无 C 依赖 | Turso beta；FTS 语法迁移；异步引擎桥接成本 |
| B. rusqlite + turbovec | rusqlite(bundled) | SQLite FTS5 | turbovec | 最成熟、FTS5 零迁移 | 存储/FTS/向量三件套；无法享受 Turso 生态 |
| C. 仅 Turso 引擎（不用 turbovec） | `turso_core` | Tantivy | Turso toy IVF | 最简洁（单组件） | **向量索引不成熟，召回性能/质量不可接受** |

**推荐 A**，理由：
1. 用户已确认选择 Turso 底层引擎；`turso_core` 提供持久化 + Tantivy FTS 一体，架构简洁；
2. turbovec 的 allowlist 混合检索与 pi-memory 的多路召回天然契合，且弥补 Turso 原生向量索引不成熟（toy 级）的缺口；
3. SQLite 文件格式兼容 = 数据迁移路径 + 逃生舱，抵消 beta 风险；
4. 方案 C 虽最简洁但向量能力不可用，不满足"召回质量与性能都要"的目标。

---

## 5. 范围划分（建议分期）

### M1：工程骨架 + Rust 核心替换（行为等价）

- **工程骨架**：cargo workspace（core lib + bin）、rust-toolchain 锁定、JSON-RPC 协议（版本握手）、TS 客户端（常驻进程 spawn/超时/崩溃重启/诊断，参照 oh-my-pi 加载器隔离思路）；turso_core 异步引擎由 Rust 侧自管 tokio runtime。
- `turso_core` 存储 + Tantivy FTS + 会话状态；recall / retain / consolidate 规则版移植（关键数值/排序逻辑与 TS 参考**逐位一致**）；TS 薄壳 + JSON-RPC 桥接（不接 LLM 的纯规则路径）。
- 现有 SQLite 库读取验证（Guarantee 2）；FTS5 → Tantivy 语法迁移。
- 验收：现有 `bun test` 套件中与规则路径相关的用例在 Rust 侧等价通过；`/memory` 命令可用；协议握手与进程生命周期验证。

### M2：语义 embedding + turbovec 索引

- ort + all-MiniLM；turbovec IdMapIndex 接入；Tantivy FTS → allowlist → 精排混合召回；模型缺失降级。
- 验收：`PI_MEMORY_EMBEDDING_PROVIDER` 语义切换为「本地模型 / 关闭」；召回质量抽样对比优于 local-hash；召回延迟指标达标（见 §6）。

### M3：LLM 代理 + 收尾

- LLM 提取/整合经 TS 代理；迁移工具；`/memory diagnose` 完善；多平台构建产物验证。

---

## 6. 验收标准

1. **行为等价**：M1 完成后，现有测试（`bun test`）中规则路径用例通过；retain/recall/consolidate 语义与 v0.1.2 一致。
2. **FTS 迁移**：现有记忆库 Tantivy FTS 重建后，关键词召回命中与 SQLite FTS5 基线一致（允许 BM25 排序差异，不允许漏召回）。
3. **召回质量**：对同一组 prompt，本地语义 embedding 的 Top-K 命中相关性显著优于 `local-hash`（抽样人工评估）。
4. **性能**：万级记忆量级下，完整 recall 注入延迟（embedding + 检索 + 合并）≤ 300ms（本地模型冷启动除外）；检索延迟不随记忆量线性恶化。
5. **降级路径**：二进制缺失、模型缺失、LLM 缺失三种场景均不阻塞 pi 主流程，且 `/memory diagnose` 能给出明确状态。
6. **数据兼容**：现有 SQLite 记忆库可直接迁移/读取，无数据丢失。

---

## 7. 风险与开放问题

| # | 风险/问题 | 影响 | 对策 |
|---|---|---|---|
| 1 | **Turso beta，API 可能变动** | 编译/行为回归 | 锁版本 + 存储 trait 隔离；SQLite 兼容格式保证回退路径 |
| 2 | **FTS5 → Tantivy 语法迁移** | 召回主路径重写 | M1 以"不漏召回"为门槛；Tantivy BM25 质量只高不低 |
| 3 | **turso_core 异步引擎与 NAPI 桥接** | 线程模型复杂度 | block_on / 专用工作线程池封装；同步 driver（bindings/rust 已有 sync 文档）优先 |
| 4 | NAPI 多平台构建与 ABI 维护 | 分发复杂 | 已规避：子进程方案下 Rust 是普通 bin，仅平台匹配；leaf 包脚本（参照 oh-my-pi） |
| 5 | turbovec 要求 rust-version ≥ 1.89 与特定 SIMD | 低版本工具链无法编译 | 构建环境锁定 toolchain；CI 验证 |
| 6 | all-MiniLM ~90MB 模型分发 | 安装体积/首启下载 | 按需下载 + 路径可配；缺失即降级 |
| 7 | Rust 侧规则提取与现有 TS 规则行为差异 | 行为回归 | M1 阶段以测试等价性为门槛 |
| 8 | 桥接层异常（超时/崩溃） | 主流程卡顿 | 全链路超时 + 失败降级 + 单次错误隔离 |
| 9 | turbovec 量化对召回质量的影响（4-bit） | 精排分数有损 | 4-bit 起步，必要时支持 2/3-bit 与纯 float 对照 |
| 10 | turso_core 依赖体积（features） | 二进制膨胀 | 裁剪 features：`fs + json + fts`，去掉 encryption/percentile 等 |
| 11 | embedding 模型加载健壮性（onnxruntime 已知问题，如特定运行时 segfault） | 崩溃/启动失败 | 参照 mnemopi：延迟加载、模型缓存目录、失败降级、加载隔离 |

---

## 8. 待确认事项

1. **新仓库确认**：独立仓库 `pi-memory-rust`（本地与 pi-memory 同级）是否接受？设计文档是否随迁至新仓库？
2. D1 桥接方式是否接受 NAPI（需 Rust 工具链与 CI 构建矩阵）？
3. M1 阶段是否**不保留** TS 实现作为降级后端（接受「二进制缺失时不可用 + 诊断提示」）？
4. 首版平台范围是否仅 macOS（arm64）？
5. embedding 模型是否接受 all-MiniLM-L6-v2（384 维）作为默认？
6. turso_core 版本锁定策略与 features 裁剪（`fs + json + fts`）是否接受？
7. 是否接受「FTS 从 SQLite FTS5 迁移到 Tantivy 语法」这一重写成本？
