# pi-memory-rust 性能与生命周期优化实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `subagent-driven-development` 按任务执行本计划。步骤使用 checkbox（`- [ ]`）语法追踪。

**Goal:** 让 pi-memory-rust 不因默认 LLM 总结、同步模型初始化、无效扫描或失控的子进程生命周期而明显拖慢 Pi 的启动、首个请求和关闭，同时保持可选总结、规则降级和重启后的语义召回能力。

**Architecture:** TS 侧负责会话生命周期、退出询问、LLM 调用和 RPC 客户端超时；Rust 子进程负责存储、规则路径、embedding 和向量索引。退出时默认不总结，只有用户明确选择后才在有界时间内执行 LLM consolidate；Rust 服务端启动阶段只创建轻量运行时，模型下载/加载在后台进行，项目首次打开时从 DB 恢复向量索引。

**Tech Stack:** TypeScript/Bun tests、Rust 2021、Cargo workspace、turso_core、Tantivy FTS、turbovec、ort、stdio length-prefixed JSON-RPC。

## Global Constraints

- 子进程 JSON-RPC 是既定桥接方式，不能改回 NAPI。
- Pi 扩展工厂不得启动后台资源；资源只能在 `session_start` 或实际需要时启动，并在 `session_shutdown` 清理。
- LLM 总结是可选质量增强，不是退出必需步骤；非交互场景默认不总结。
- 用户未选择总结、确认框超时、LLM 超时、Rust RPC 超时或子进程异常都不能阻塞 Pi 退出。
- 保持现有数据库布局和 SQLite/Turso 兼容格式；不做无关存储重构。
- 结构化表当前位于每个 projectScope 独立数据库中，因此不重复增加 project_scope 列；只在有实际语义和性能收益时调整查询。
- 每个实现任务遵循 TDD：先写一个能证明缺陷的失败测试，再写最小实现，再跑针对性测试和全量验证。

## 文件结构规划

- `ts/src/client.ts` — RPC 客户端的 single-flight 启动、请求超时、优雅关闭和进程退出状态；不负责 Pi UI 或业务总结策略。
- `ts/index.ts` — Pi 生命周期事件、退出总结确认、LLM 超时/降级和 Rust RPC 编排；不负责 Rust 存储实现。
- `ts/test/smoke.test.ts` — 真实 Rust 子进程启动、握手、优雅关闭和超时行为。
- `ts/test/lifecycle.test.ts` — 使用最小 fake Pi API 验证退出确认分支和 consolidate 触发条件。
- `crates/core/src/bin/pi-memory-server.rs` — 项目运行时池、后台模型准备、RPC 服务和关闭路径；不放 TS 交互逻辑。
- `crates/core/src/embedding_runtime.rs` — embedder 预热和项目向量索引从持久化 embedding 恢复。
- `crates/core/src/model_download.rs` — 模型文件下载超时、原子文件写入和可重入下载。
- `crates/core/src/runtime.rs` — recall/retain/consolidate 编排；删除无效全量查询，不改变排序公式。
- `crates/core/src/storage/turso.rs` — 为向量恢复提供受限的 embedding/记忆读取接口；不改变数据库文件布局。
- `crates/core/tests/vector_index.rs` — 向量索引恢复和映射一致性测试。
- `crates/core/tests/isolation.rs` — 进程/运行时重新打开后的项目隔离与召回回归测试。
- `crates/core/tests/runtime.rs` — recall 不发生无效全量读取的行为/性能辅助验证（若现有测试组织允许则新增）。
- `crates/core/src/bin/pi-memory-bench.rs` — 增加冷启动、首个 recall、恢复索引和无 LLM 关闭相关基准输出。
- `docs/superpowers/specs/2026-08-21-pi-memory-rust-session-summary-design.md` — 已批准的退出总结交互设计，仅在实现细节需要澄清时更新。
- `README.md` — 更新退出总结默认行为、非交互降级和向量索引恢复说明。
- `docs/superpowers/plans/2026-08-21-pi-memory-rust-performance.md` — 本实施计划和验收记录。

---

### Task 1: 修复 RPC 客户端优雅关闭与启动并发

**Goal:** 真实发送 `shutdown` RPC、等待子进程退出，并保证并发调用只拉起一个 Rust 子进程。

**Files:**
- Modify: `ts/src/client.ts`
- Test: `ts/test/smoke.test.ts`

**Boundaries:**
- Allowed to change: `ts/src/client.ts`, `ts/test/smoke.test.ts`
- Must not change: Rust protocol method names or Pi event behavior

**Interfaces:**
- `ensureStarted()` 保持公开签名不变。
- `shutdown()` 保持公开签名不变。
- 内部 pending 请求可以从“只 resolve”改为可 reject，但错误消息必须继续包含进程退出原因。

**Acceptance:**
- `shutdown()` 不再先清空进程引用后调用 `invoke()`。
- smoke test 能观察到正常 exit，而非必然 `SIGKILL`。
- 两个并发 `ensureStarted()` 最终只有一个子进程和一次 hello 握手。
- 请求超时会立即 settle 调用方 Promise，并随后终止失控子进程。
- 重复 shutdown、进程已退出和启动失败均快速返回或抛出明确错误。

**Stop If:**
- 需要修改 Rust 协议帧格式。
- 需要改变公开 RPC 方法名。

- [ ] **Step 1: 写失败测试**
  - Test file: `ts/test/smoke.test.ts`
  - Add: 关闭后捕获客户端日志不得出现 `SIGKILL`；增加并发 `Promise.all([ensureStarted(), ensureStarted()])` 场景；增加一个超时子进程场景验证调用在超时窗口内结束。
- [ ] **Step 2: 运行测试并确认失败**
  - Run: `bun test ts/test/smoke.test.ts`
  - Expect: 当前实现关闭路径失败或日志显示 `SIGKILL`，并发/超时断言至少一项失败。
- [ ] **Step 3: 写最小实现**
  - Change: `ts/src/client.ts`
  - Implement: `startPromise` single-flight；让 `invoke` 支持指定进程或内部统一使用当前进程；在 shutdown 中使用局部进程发送 RPC并等待 exit；timeout 回调立即 reject 当前请求；清理 pending、buffer 和生命周期状态。
- [ ] **Step 4: 运行针对性验证并确认通过**
  - Run: `bun test ts/test/smoke.test.ts`
  - Expect: 生命周期、并发启动和超时测试全部通过，正常关闭不再记录 `SIGKILL`。
- [ ] **Step 5: 执行必要的收口检查**
  - Run: `bun test ts/test/`
  - Expect: 现有 TS 测试全部通过。
- [ ] **Step 6: 记录验证并准备提交**
  - Record: 关闭耗时、正常 exit 证据、并发启动测试结果。

---

### Task 2: 退出总结改为用户选择且有界

**Goal:** 只有用户在真正退出 Pi 时明确选择“总结”才执行 LLM consolidate；取消、超时、无 UI 或其它 session 生命周期事件都直接关闭。

**Files:**
- Modify: `ts/index.ts`
- Create: `ts/test/lifecycle.test.ts`

**Boundaries:**
- Allowed to change: `ts/index.ts`, `ts/test/lifecycle.test.ts`
- Must not change: Rust consolidate 算法和数据库字段

**Interfaces:**
- `session_shutdown` 使用 `event.reason` 判断是否为 `quit`。
- UI 确认使用 Pi 的 `ctx.ui.confirm(title, message, { timeout })`。
- `runLlm` 增加可传入 timeout 的内部调用能力，超时返回失败状态而不是抛出未处理 Promise。

**Acceptance:**
- `quit + hasUI` 显示确认框，默认/超时均不总结。
- 用户确认后才执行 `llmPrompts`、`consolidateSource`、LLM provider 和 `consolidateSessionMemories`。
- `new/resume/fork/reload` 不显示确认框、不调用 consolidate。
- `ctx.hasUI === false` 不显示确认框、不调用 LLM。
- LLM 超时后仍进入 `client.shutdown()`，Pi 退出路径有明确上限。
- shutdown 清理放在 `finally` 语义中，任何总结异常都不能阻止 Rust 客户端关闭。

**Stop If:**
- 当前 Pi 类型或运行时不提供 `event.reason`/`ctx.ui.confirm`，需要先回到 spec 澄清。
- 必须修改 Pi 宿主代码才能实现确认框。

- [ ] **Step 1: 写失败测试**
  - Test file: `ts/test/lifecycle.test.ts`
  - Add: fake Pi 注册 handler，分别验证 quit/取消、quit/确认、non-quit、no-UI、LLM 超时五个分支。
- [ ] **Step 2: 运行测试并确认失败**
  - Run: `bun test ts/test/lifecycle.test.ts`
  - Expect: 当前 shutdown handler 无条件执行 consolidate，quit/non-quit 和取消断言失败。
- [ ] **Step 3: 写最小实现**
  - Change: `ts/index.ts`
  - Implement: `shouldSummarizeOnShutdown` 和 `summarizeSessionOnShutdown` 内部逻辑；确认框采用短 timeout；LLM 使用 Promise race/AbortSignal 能力和短硬超时；将 client shutdown 放入最终清理路径。
- [ ] **Step 4: 运行针对性验证并确认通过**
  - Run: `bun test ts/test/lifecycle.test.ts`
  - Expect: 五个分支通过，超时不会悬挂。
- [ ] **Step 5: 执行必要的收口检查**
  - Run: `bun test ts/test/`
  - Expect: 全部 TS 测试通过。
- [ ] **Step 6: 记录验证并准备提交**
  - Record: 用户取消/确认、非 quit、无 UI 和超时的执行路径。

---

### Task 3: 删除 recall 无效扫描并限制首次请求成本

**Goal:** 删除 recall 中不参与结果的 working/episodic 全量读取，并避免模型下载/加载同步阻塞 RPC 主循环。

**Files:**
- Modify: `crates/core/src/runtime.rs`
- Modify: `crates/core/src/bin/pi-memory-server.rs`
- Modify: `crates/core/src/embedding_runtime.rs`
- Modify: `crates/core/src/model_download.rs`
- Test: `crates/core/tests/runtime.rs`（若需新增）

**Boundaries:**
- Allowed to change: 上述 Rust 文件及对应测试
- Must not change: recall 权重、候选合并顺序、embedding 模型格式

**Interfaces:**
- `build_recall_injection` 保持输入输出和排序行为不变。
- `SharedEmbedder` 增加幂等后台预热入口，失败时继续返回 None 走 FTS-only。
- 模型下载保持 `ensure_model_dir` 同步 API 兼容，但每个 HTTP 请求必须有全局 timeout；服务端启动/prepare-model 的调用策略可异步化。

**Acceptance:**
- `build_recall_injection` 不再调用两个无用 list 查询。
- 模型文件已就绪时，server 启动后台 warmup，不阻塞 hello 或首个非 embedding RPC。
- 模型未就绪时，后台下载有超时、临时文件不会被当作完整模型；失败后业务继续 FTS-only。
- `/memory prepare-model` 不会无限等待，重复触发不会并发下载同一文件。
- 现有 recall 行为测试和 benchmark 结果不回退。

**Stop If:**
- ureq 当前版本无法提供可靠 timeout 而需要更换 HTTP 库。
- 后台模型线程无法安全与 ort Session 生命周期配合；此时保留 FTS-only，停止扩展模型并行化。

- [ ] **Step 1: 写失败测试**
  - Test file: `crates/core/tests/runtime.rs` 或现有 runtime 测试位置
  - Add: 针对模型下载超时/部分文件和 FTS-only 降级的测试；用现有 recall 测试保护输出。
- [ ] **Step 2: 运行测试并确认失败**
  - Run: `cargo test -p pi-memory-core --test runtime`（若没有该测试目标则运行对应现有测试目标）
  - Expect: 新增下载/降级断言在当前实现下失败。
- [ ] **Step 3: 写最小实现**
  - Change: Rust files above
  - Implement: 删除无效查询；为下载配置 timeout 和独占/原子写入；在 server 启动和下载完成后触发一次后台 embedder warmup；prepare-model 改为幂等后台任务或明确 bounded response。
- [ ] **Step 4: 运行针对性验证并确认通过**
  - Run: `cargo test --workspace`
  - Expect: Rust 全部测试通过。
- [ ] **Step 5: 执行必要的收口检查**
  - Run: `cargo clippy --workspace --all-targets -- -D warnings`
  - Expect: 无 clippy error；现有 unused variable、new_without_default 等警告必须一并清理。
- [ ] **Step 6: 记录验证并准备提交**
  - Record: recall 前后基准、模型未就绪降级、下载超时结果。

---

### Task 4: 项目首次打开时恢复向量索引

**Goal:** Rust 子进程重启或项目库重新打开后，从 `memory_embeddings` 恢复 turbovec 索引和 memory 内容映射，不让语义召回退化为当前进程新写入数据 בלבד。

**Files:**
- Modify: `crates/core/src/embedding_runtime.rs`
- Modify: `crates/core/src/bin/pi-memory-server.rs`
- Modify: `crates/core/src/storage/turso.rs`
- Test: `crates/core/tests/vector_index.rs`
- Test: `crates/core/tests/isolation.rs`

**Boundaries:**
- Allowed to change: 上述 Rust 文件及测试
- Must not change: `memory_embeddings` 表格式和现有向量维度/量化配置

**Interfaces:**
- `ProjectVectors::load_from_storage(storage)` 或等价构造接口负责一次性恢复。
- 恢复时只载入有效的 working/episodic embedding；无法找到正文的记录跳过并记录诊断信息。
- 恢复后的 id 映射、content_map 和 next_id 必须一致，重复恢复不能重复添加。

**Acceptance:**
- retain 写入后销毁内存 runtime，再从同一 DB 创建新 `ProjectVectors`，仍能执行 embedding candidate search。
- 恢复不会跨 project DB 读取数据。
- 空库、损坏 embedding JSON、维度不符时不阻塞 FTS-only recall。
- 现有 `VectorIndex::sync` 测试继续通过；如果不采用 sync 文件，删除误导性注释并以 DB hydrate 作为唯一恢复路径。

**Stop If:**
- turbovec 不能安全接受从 DB 读取的向量或无法恢复外部 id；改为使用现有 `IdMapIndex` 的稳定外部 id 方案前先停下报告。

- [ ] **Step 1: 写失败测试**
  - Test file: `crates/core/tests/vector_index.rs`, `crates/core/tests/isolation.rs`
  - Add: 新 runtime 从 DB 恢复后搜索命中；无效向量不使恢复失败；跨项目恢复隔离。
- [ ] **Step 2: 运行测试并确认失败**
  - Run: `cargo test -p pi-memory-core --test vector_index --test isolation`
  - Expect: 当前 `ProjectVectors::new()` 没有 hydrate，重启恢复断言失败。
- [ ] **Step 3: 写最小实现**
  - Change: storage/embedding_runtime/server
  - Implement: 增加受限读取 memory embedding 和正文的方法；项目首次打开后调用 hydrate；对旧模型/维度/坏 JSON 跳过；维护映射与向量 id。
- [ ] **Step 4: 运行针对性验证并确认通过**
  - Run: `cargo test -p pi-memory-core --test vector_index --test isolation`
  - Expect: 恢复、坏数据降级、物理隔离全部通过。
- [ ] **Step 5: 执行必要的收口检查**
  - Run: `cargo test --workspace`
  - Expect: 全 Rust 测试通过。
- [ ] **Step 6: 记录验证并准备提交**
  - Record: 重启前后召回结果和索引恢复耗时。

---

### Task 5: 补充性能基准、文档和最终质量门禁

**Goal:** 用可重复数据证明插件不会默认阻塞 Pi 启动/关闭，并让文档与实际策略一致。

**Files:**
- Modify: `crates/core/src/bin/pi-memory-bench.rs`
- Modify: `README.md`
- Modify: `docs/superpowers/plans/2026-08-21-pi-memory-rust.md`
- Modify: `docs/superpowers/specs/2026-08-21-pi-memory-rust-session-summary-design.md`（仅记录批准状态和实现约束）
- Test: `ts/test/smoke.test.ts`, `ts/test/lifecycle.test.ts`

**Boundaries:**
- Allowed to change: 上述 benchmark、文档和测试
- Must not change: 无关产品功能或发布版本号

**Acceptance:**
- benchmark 输出 cold spawn/hello、首个项目 RPC、首个 recall、重复 recall、hydrate 后 recall 的时间。
- shutdown benchmark 至少覆盖“不总结”路径，目标是关闭不等待 LLM。
- README 明确：退出默认不总结，只有用户确认才总结，非交互模式直接跳过；模型缺失为 FTS-only；向量索引从 DB 恢复。
- 原有 plan 中 NAPI 残留文案修正为子进程 JSON-RPC。
- `git diff --check`、`cargo test --workspace`、`bun test ts/test/`、`cargo clippy --workspace --all-targets -- -D warnings` 全部通过。

**Stop If:**
- 基准显示目标机器上 no-summary shutdown 仍有可重复的明显等待，停止宣称完成并报告具体等待来源。

- [ ] **Step 1: 写失败测试/基准断言**
  - Test files: `ts/test/smoke.test.ts`, `ts/test/lifecycle.test.ts`
  - Add: 关闭路径无 LLM 调用、正常 exit、重复启动单进程的回归断言。
- [ ] **Step 2: 运行测试并确认失败**
  - Run: `bun test ts/test/`
  - Expect: 在任务 1/2 完成前，旧生命周期日志或无选择总结断言失败。
- [ ] **Step 3: 写最小实现与文档**
  - Change: benchmark、README、旧 plan 和批准 spec 的状态/说明。
- [ ] **Step 4: 运行全量验证**
  - Run: `cargo test --workspace && bun test ts/test/`
  - Expect: Rust/TS 测试零失败。
- [ ] **Step 5: 执行静态和差异检查**
  - Run: `cargo clippy --workspace --all-targets -- -D warnings && git diff --check && git status --short`
  - Expect: clippy 零错误、diff 无空白错误，只有本次相关文件变化。
- [ ] **Step 6: 记录最终性能证据**
  - Record: 关闭 no-summary 耗时、确认总结耗时上限、冷启动/首个 recall/恢复召回数据，并据此更新验收结论。
