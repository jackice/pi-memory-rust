# pi-memory-rust 审阅报告

> 基线：`ea66107`（v0.1.16，2026-10-28）
> 审阅方式：静态审阅（`runtime.rs` / `turso.rs` / `embedding_runtime.rs` / `embedding.rs` / `model_download.rs` / `pi-memory-server.rs` / `llm.rs` / `extraction.rs` / `recall.rs` / `consolidation.rs` / `ts/index.ts` / `ts/src/client.ts` / 测试 / README / 设计文档），并用临时 probe 复核了 Turso FTS 行为。**未跑完整测试套件**，性能数字均来自仓库内 benchmark 的既有输出，不代表审阅时重新测量。
> 配套文档：可执行计划见 [`docs/REPAIR_PLAN.md`](./REPAIR_PLAN.md)。本文负责“问题是什么、证据在哪、为什么重要”；计划负责“怎么改、先改什么、如何验收”。
> **编号约定**：本文只保留“问题/修复方向”；具体实施编号以 `REPAIR_PLAN.md` 5.1 的 PR 速查表为准（中文 FTS = PR-1.7，独立向量 = PR-1.6，clear/迁移 = PR-2.6）。

---

## 0. 结论

**架构方向是对的，但当前版本还不能算“可靠”。**

它已经具备一个完整记忆系统该有的骨架：working / episodic / structured 四类记忆、FTS + 向量 + 结构化多路召回、Rust 常驻服务、TS 插件生命周期、迁移和诊断命令。问题不在于“选错了技术栈”，而在于**关键路径上的正确性还没有闭合**：

1. **写入会丢**：同一 turn 的多条 fact/preference/instruction/timeline 共用固定 ID，后写覆盖先写；retain 半失败后还会因为“working 已存在”直接早退，永久不再补。
2. **中文检索基本不可用**：CJK prompt 被整体包成一个 FTS phrase；FTS 没有 `LIMIT`；结构化召回是 `LIKE %整句%`。
3. **语义召回被 FTS 候选集绑死**：向量只在 FTS 命中集合里重排；FTS 为空时只取 1 条；episodic 根本没有写入向量。
4. **长期记忆依赖“优雅退出 + 用户确认”**：只有 `reason === "quit"` 且用户点确认才做 consolidation；headless、崩溃、直接关窗口都不会产生 episodic。

在这四条修好之前，调 Top-6、换 embedding 模型、做记忆 UI 的收益都会被掩盖。

**审阅结论：方向正确，实施顺序需要调整；建议先发一个 v0.1.17 止血版，再做召回重构。** 详见第 5、6 节。

---

## 1. 系统在做什么

```
turn_end
  └─ 同步调用 LLM 提取 → retainTurnMemories
        ├─ working_memory（整轮文本）
        └─ facts / preferences / instructions / timeline

recall
  ├─ FTS(working, episodic) —— 带 project_scope 过滤
  ├─ 向量重排 —— 仅允许 FTS 候选（allowlist）
  └─ structured LIKE —— 整句 LIKE
        └─ recall_memories 合并 → 注入 6 条

session_shutdown(reason=quit) + 用户确认
  └─ LLM 摘要 → consolidate_session_memories → episodic_memory
```

TS 侧负责 Pi 生命周期、LLM 调用和 RPC；Rust 侧负责存储、检索、提取规则和向量索引。这个分层是合理的，问题集中在“边界条件”和“失败路径”。

---

## 2. P0：会导致数据丢失或核心能力失效

### P0-1 同一 turn 的多条结构化记忆互相覆盖

**证据**：`crates/core/src/runtime.rs:143-187`

```rust
fact.fact_id = format!("{memory_id}:fact:llm");
preference.id = format!("{memory_id}:preference:llm");
instruction.id = format!("{memory_id}:instruction:llm");
event.event_id = format!("{memory_id}:timeline:llm");
```

同一 turn 内所有 fact 共享同一个 ID，`insert_facts` 又是 upsert 语义（`ON CONFLICT ... DO UPDATE`），因此 N 条 fact 最终只剩 1 行。用当前代码对同一 turn 传入 3 条 fact 的 LLM JSON，`count_by_session("facts") == 1`，已复现。

**影响**：用户一轮里说“以后用中文、不要改 A 文件、提交前跑测试”，可能只留下最后一条。这是静默的数据丢失，不是排序问题。

**修复方向**：内容寻址 ID（`{turn_id}:{kind}:{hash16}`），忽略 LLM 返回的 ID；同一 turn 内容相同则天然去重，重试天然幂等。见计划 PR-1.1 / ADR-1。

### P0-2 retain 不是事务，失败后还会永久早退

**证据**：`crates/core/src/runtime.rs:101-110`

```rust
if storage.get_working(&memory_id)?.is_some() {
    return Ok(());
}
```

后面才依次写 working → structured → session_state，没有事务。只要 working 写成功、structured 写失败（进程退出、DB 错误、JSON 解析后的插入错误），下一次重试会因为 working 已存在而直接返回 `Ok(())`，structured 永远缺失。

**影响**：产生“看起来 retain 成功、实际只有原文没有结构化记忆”的半成品；而且无法通过重试修复。

**修复方向**：working + structured + session_state 单事务；增加 `metadata.retain` 完成标记（`working` / `rule_complete` / `llm_complete` / `failed` / `unrecoverable`），并用 `source_sha` 判断是幂等重试还是“同一 turn 文本已变化”；后者要替换 structured 并让旧 embedding 失效。旧半成品只能用 `working.content` 重放规则提取，不能假装恢复了 LLM-only 结果。见计划 PR-1.2 / ADR-2。

### P0-3 中文 FTS 实际上不可用

**证据**：

- `crates/core/src/runtime.rs:381-392`：`to_safe_fts_query` 只按空白切词。中文 prompt 没有空格，于是整段被包成一个 phrase：`"怎样避免多个智能体同时修改代码冲突"`。FTS 要求精确短语命中，子串“中文”“向量”都搜不到。
- `crates/core/src/storage/turso.rs:497-518`：FTS SQL 没有 `LIMIT`，且 `WHERE fts_match(...) AND project_scope = ?` 的额外谓词会让 Turso 无法把 `ORDER BY/LIMIT` 下推（计划 1.2 风险 A 已用 `EXPLAIN QUERY PLAN` 复核）。
- `crates/core/src/storage/turso.rs` 的结构化召回使用 `LIKE %整句%`，长 prompt 几乎不可能命中短 fact。
- `crates/core/tests/turso_storage.rs` 里甚至有一条测试**显式断言中文子串不命中**，说明这不是“未覆盖”，而是当前行为被接受了下来。

**影响**：中文用户的核心体验（“向量”“中文”“并行开发”）直接失效；长 prompt 的结构化召回也失效。

**修复方向**：写入时物化 `search_text`（英文词 + CJK bigram），查询侧用同一套 `terms()` 生成 `"t1" OR "t2"`；FTS 查询只保留 `fts_match` + `ORDER BY` + `LIMIT`，scope 靠 DB 物理隔离；structured 改为按词项 OR LIKE。实施：`search_text`/CJK/FTS Top-K 在 **PR-1.7（M1，先行）**，structured 词项召回在 PR-2.3，PR-2.1 只做真实数据复核与 `ngram` spike。

### P0-4 向量召回被 FTS 候选集限制，episodic 没有向量

**证据**：

- `crates/core/src/embedding_runtime.rs:341-376`：`build_embedding_candidates` 先构造 FTS 候选的 `allowlist`，再 `search_with_allowlist`；`allowlist.is_empty()` 时用 `fts_candidate_memories.len().max(1)`，也就是**FTS 为空时只取 1 条**。
- `crates/core/src/runtime.rs:233-292`：`consolidate_session_memories` 只 `insert_episodic`，没有调用 `save_embedding`；episodic 永远不进向量索引。
- `crates/core/src/embedding_runtime.rs:278-311`：`save_embedding_vector` 是“先删旧向量、再加新向量”，add 失败会留下“DB 有、索引无”的空洞；batch 版本 `save_embeddings_batch` 则直接 `index.add`，不清理旧 ID，重复回填会产生孤儿向量。

**影响**：语义召回名存实亡。用户换一种说法（“怎么避免多个 agent 改同一个文件” vs “Git worktree 隔离”）时，只要 FTS 没命中，向量也不会救场；长期记忆（episodic）更是完全不可语义检索。

**修复方向**：向量全局 Top-K，不传 allowlist；episodic 在 consolidation commit 后写向量；批量写入改为 add-before-delete + `needs_rehydrate`；提供缺失向量回填。实施：episodic 向量/回填在 PR-1.4，独立向量 Top-K 在 **PR-1.6（M1，依赖 PR-1.7）**，完整 RRF 在 PR-2.2b（M2）。

---

## 3. P1：会显著降低可靠性、隐私或可维护性

### P1-1 `turn_end` 只保留 assistant 文本，且同步等 LLM 60 秒

**证据**：`ts/index.ts:172-189`

```ts
const text = extractText(event.message);
const { result, failed } = await runLlm(`${prompts?.extract ?? ""}\n\n${text}`, ctx);
await client.call("retainTurnMemories", { ... });
```

- `event.message` 是 assistant 消息；用户这一轮说的“以后都用中文 / 不要改这个文件”不一定出现在 assistant 输出里。
- `TURN_LLM_TIMEOUT_MS = 60_000`（`ts/index.ts:53`），`turn_end` 被 Pi await，最坏情况下每轮收尾阻塞 60 秒。
- 没有持久化队列：进程崩溃/强退时，这一轮直接丢失。

**影响**：偏好/指令召回率低；交互卡顿；崩溃丢 turn。

**修复方向**：`before_agent_start` 缓存 user prompt，`turn_end` 组合 user + assistant 入持久化 outbox；后台 worker 做 LLM 提取；`turn_end` 同步路径 <100ms。见计划 PR-3.3 / ADR-5。

### P1-2 Consolidation 只在“quit + 用户确认”时发生

**证据**：`ts/index.ts:59-71,125-147`

```ts
if (event.reason !== "quit") return false;
if (!hasShutdownUi(ctx)) return false;
return await ctx.ui.confirm("Summarize this session before exiting?", ...);
```

headless 环境没有 UI，直接返回 false；用户点“否”也返回 false；崩溃、`session_end` 以外的退出路径都不会触发。结果是：**只要用户不优雅退出并确认，就永远没有 episodic memory**。

**影响**：长期记忆实际上依赖一个用户不一定知道、也不一定会同意的操作；episodic 表可能长期为空。

**修复方向**：consolidation 改为后台自动维护（例如 pending 清空且未 consolidate 的 working 达到阈值时触发），`session_shutdown` 只做有界 flush；确认只用于删除类破坏性操作，不用于“是否保存记忆”。见计划 PR-3.5。

### P1-3 `/memory clear <target>` 会清掉当前项目的全局表

**证据**：`crates/core/src/runtime.rs:349-353` + `crates/core/src/storage/turso.rs:980-995`

```rust
let scope = if target.is_empty() { project_scope } else { target };
storage.clear_project_memories(scope)?;
```

```rust
for table in ["working_memory", "episodic_memory"] {
    DELETE FROM {table} WHERE project_scope = ?1
}
// Clear global tables (matches the TS clear)
for table in ["memory_embeddings", "facts", "preferences", "instructions", "timeline", "session_state"] {
    DELETE FROM {table}
}
```

`facts/preferences/instructions/timeline/session_state/embeddings` 没有 `WHERE project_scope`。用户想清目标项目，结果当前项目的结构化记忆和所有向量一起被清空。

**影响**：破坏性数据丢失，且与命令语义不符。

**修复方向**：所有表都按 scope 删除；target 与当前 project 不同时通过 server pool 打开目标项目再清；全项目清理必须显式 `all` + 二次确认；clear 的 DB 删除在单事务内完成，索引重建失败返回 `index_state=partial` 而不是假装回滚。见计划 PR-2.6（从 M4 前移）。

### P1-4 迁移会把所有 project scope 导入同一个 DB

**证据**：`crates/core/src/storage/turso.rs:298-356`：`migrate_from` 调用 `list_all_working/episodic/facts/...`，没有 scope 过滤；`TursoStorage::open(source_path)` 还会执行 `init()`，可能改写源库。迁移完成后所有 scope 混在一个 DB 里，之后 `diagnose` 只能靠“单 DB 多 scope”慢路径兜底。

**影响**：跨项目记忆泄漏；召回可能混入其他项目的偏好；源库被意外修改。

**修复方向**：迁移前选择目标 scope，只导入该 scope（或按 scope 拆分目标 DB）；源库只读打开；导入在目标 DB 的单事务内并校验计数；FTS/向量 rebuild 在事务外，失败返回 `index_state=partial`、可幂等重试，不笼统声称“整体回滚”。见计划 PR-2.6；PR-4.3 只负责发布说明。

### P1-5 结构化记忆的时间戳全是 1970

**证据**：`crates/core/src/core/extraction.rs:9,58,74,91,114` 对所有规则提取项写入 `EPOCH_ISO = "1970-01-01T00:00:00.000Z"`；`runtime.rs:151,162,173,185` 在 LLM 结果缺时间戳时也补 1970。

**影响**：timeline 排序、`created_at DESC`、按时间衰减/清理全部失真；用户看到“1970 年的事件”。

**修复方向**：使用 turn 的实际时间（`now_iso()` / working.created_at），保留 `EPOCH_ISO` 只作为“未知时间”的兼容常量；规则提取测试同步更新。见计划 PR-1.5。

### P1-6 全局 Mutex + 2 秒 RPC 超时 + 请求路径同步加载模型

**证据**：

- `crates/core/src/bin/pi-memory-server.rs:135-140,247-251`：`Arc<Mutex<SharedState>>` 包住整个 storage pool；一个慢请求（migrate/prepareModel/大项目 recall）会阻塞所有项目。
- `ts/src/client.ts:22-23,376-394`：默认超时 2000ms；超时后 socket 模式 `conn.destroy()`，stdio 模式 `SIGKILL`。同一连接上的其他 pending 请求一起失败。
- `crates/core/src/embedding_runtime.rs:119-156`：`ensure_loaded()` 在 `Unloaded/Failed` 时**在请求线程同步加载 ONNX**。首次 recall/retain 很可能超过 2s，触发上面的销毁路径。
- `crates/core/src/bin/pi-memory-server.rs:175-181`：`conn_count.fetch_sub` 在 `handle_socket_conn` 返回后才执行，线程 panic 时不会减；`state.lock().expect("mutex poisoned")` 一旦 poison 会继续 panic。连接数泄漏会让 server 永不退出，锁 poison 会让后续请求全挂。

**影响**：用户感知为“记忆偶尔没写进去/召回为空”，而不是明确报错；多 session 互相拖慢；孤儿进程。

**修复方向**：per-project lock；请求路径只使用 `Ready` embedder，模型加载走后台 warmup/显式 `prepareModel`；按方法分级超时，超时只 reject 当前请求、不销毁连接；处理 mutex poison（或改 `parking_lot`）；`conn_count` 用 guard/`catch_unwind` 保证递减。见计划 PR-1.4 / PR-3.1 / PR-3.2 / ADR-4/6。

### P1-7 模型文件只检查“存在”，损坏后每个请求都重试加载

**证据**：`crates/core/src/model_download.rs:22` 只检查 `model.onnx` 和 `tokenizer.json` 是否存在；下载没有校验和/大小校验（`download_file` 只是写 `.part` 再 rename）。`crates/core/src/embedding_runtime.rs:65-100,126-127` 把 `Failed` 当作可重试状态，**每次请求**都会再次同步尝试加载损坏文件。

**影响**：半截文件/镜像返回 HTML 也能“就绪”，随后 ONNX 加载失败；每个 recall/retain 都重复一次昂贵的失败加载，叠加 2s 超时形成“记忆全空 + 卡顿”的循环。

**修复方向**：版本化 manifest（固定 upstream revision + 每个文件的 size/SHA-256）作为唯一预期值，下载/本地文件都与 manifest 比对；失败进入带退避的 `Failed`，由后台 warmup/`prepare-model` 重试，请求路径不重试；`VERIFY=0` 仅开发/本地构建可用，仍强制结构/维度/tokenizer 检查；提供 `model-status` 诊断。见计划 PR-1.4。

### P1-8 embedder 的 tokenization 假设不成立

**证据**：`crates/core/src/embedding.rs:58-81`

```rust
// encode_batch pads automatically to the longest sequence in the batch
let encodings = self.tokenizer.encode_batch(texts.to_vec(), false)?;
let padded_len = encodings[0].len();
```

- `encode_batch(..., false)` 明确**不加 special tokens**；all-MiniLM 的 CLS/SEP 被省掉，池化语义与训练时不一致。
- `tokenizers` 默认不 padding/truncation（除非 tokenizer.json 显式配置）；不同长度文本的 batch 会得到不同长度，按 `encodings[0].len()` 构造 `[batch, padded_len]` 张量会在运行时报形状错误。长文本也没有显式截断到模型最大长度。

**影响**：批量回填/批量 embedding 可能直接失败；单条 embedding 质量下降；超长 working 文本可能超出模型位置上限。

**修复方向**：显式 `with_padding` / `with_truncation(max_len)`，`add_special_tokens=true`；对超长文本记录并截断；测试覆盖不同长度 batch。见计划 PR-1.4 / PR-2.5。

---

## 4. P2：质量、成本与文档

| 问题 | 证据 | 影响 | 建议 |
|---|---|---|---|
| 召回没有相关性阈值 | `runtime.rs:57-63` 固定 `limit: 6`；`recall.rs` 只按分数排序 | 无关记忆也会被注入，浪费 token、干扰模型 | RRF 后加最低分/相对阈值；`diagnose` 输出丢弃原因（PR-2.2b / 4.1） |
| consolidated working 仍参与召回 | 召回只按 FTS/向量取候选，没有 `consolidated_at IS NULL` 过滤 | 同一内容以 working + episodic 重复注入 | M2 用 per-project eligibility cache 排除，避免 SQL 过滤破坏 FTS LIMIT 下推（PR-2.4） |
| 合并是线性查找 | `core/recall.rs:80-95` 每个候选 `merged.iter().position(...)` | 候选数上来后 O(n²)，且 RRF 需要按 ID 聚合 | RRF 改用 `HashMap<(kind,id)>` 聚合（PR-2.2b） |
| `content_map` 无界增长 | `embedding_runtime.rs:199,254,298,333` 保存所有 working/episodic 原文 | 长时间运行内存随项目历史线性增长 | 只在索引中保存引用，或按项目 LRU/容量上限裁剪（PR-3.1） |
| benchmark 没测到 hybrid | `pi-memory-bench.rs:142-150` 用英文 `how about X?` 查中文内容；只创建 working | 5.12ms 不能证明中文/混合召回可用 | 中英文 query 集 + 输出候选来源/数量（PR-4.2） |
| README 过度承诺 | README 声称 hybrid recall / “FTS blind-spot 测试” | 用户和贡献者被误导 | 修文档，删掉不存在的测试描述（PR-4.3） |
| Windows socket 语义不一致 | `client.ts` socket 模式不读 stdout；server 在 Windows 回退 stdio | Windows 上可能完全不可用 | 短期禁用 socket + 文档标注 experimental（PR-3.1） |
| 隐私与权限 | DB 文件、日志、socket 没有统一的 0700/0600 约定；记忆内容可能进 debug log | 同机其他用户可读；日志泄漏用户内容 | DB 目录 0700、文件 0600；日志脱敏；README 增加隐私说明（PR-3.1 / 4.3） |
| session 回退语义 | `ts/index.ts:226-230` 找不到 sessionManager 时回退 `"default-session"` | 多 session/subagent 共享同一 ID 空间，`turnIndex` 可能冲突 | 明确 fallback 策略；`(session_id, turn_index)` 唯一性加测试（PR-3.3） |

---

## 5. 对 `REPAIR_PLAN.md` 的评估

### 5.1 方向正确

计划的核心判断——“先修数据正确性和召回覆盖，再谈质量和成本；不换存储/向量引擎”——与本次审阅一致。ADR-1（内容寻址 ID）、ADR-2（事务边界）、ADR-3（双路 Top-K + RRF）、ADR-5（outbox + lease）都是正确的方向，测试矩阵也覆盖了主要失败路径。

### 5.2 需要调整的三处优先级

1. **中文 `search_text` 应前移到 M1/止血版（当前编号 PR-1.7）。**
   中文 FTS 不是“质量优化”，而是当前核心功能不可用。如果 v0.1.17 只修写入、不修中文，用户升级后仍然搜不到“向量/中文”，止血版的感知价值很低。

2. **独立向量 Top-K 的最小实现应前移到 M1（当前编号 PR-1.6）。**
   完整 RRF 可以留在 M2（PR-2.2b），但“FTS 为空时向量只返回 1 条”必须先在 M1 修掉。否则即使 episodic 写入了向量，FTS 没命中时仍然召回不到。拆分为：
   - **PR-1.6（M1，依赖 PR-1.7）**：去掉 allowlist、全局 Top-K、FTS 为空时返回 K 条；暂不引入 RRF，先保证“能召回”。
   - **PR-2.2b（M2）**：完整三路 RRF、权重、阈值、确定性 tie-break。

3. **consolidated working 的排除/去重应从 M4 前移到 M2。**
   一旦 consolidation 开始自动产生 episodic（PR-3.5），同一内容会同时以 working 和 episodic 出现；如果等到 M4 才去重，中间版本会持续重复注入。建议在 PR-2.4 的 eligibility 过滤里一起做。

### 5.3 计划里缺失、本次补入的项

| 缺失项 | 建议落点 |
|---|---|
| consolidation 触发策略（quit + confirm 之外） | 新增 PR-3.5 |
| `/memory clear <target>` 清全局表 | 新增 PR-2.6（前移） |
| 迁移不按 scope 过滤、源库被改写 | PR-2.6（PR-4.3 仅发布说明） |
| 结构化时间戳 1970 | 新增 PR-1.5 |
| 模型文件校验和 / `Failed` 重试循环 | PR-1.4 |
| Mutex poison / `conn_count` 泄漏 | PR-3.1 |
| `content_map` 无界增长 | PR-3.1 / PR-4.1 |
| embedder special tokens / padding / truncation | PR-1.4 / PR-2.5 |
| RRF 合并 O(n²) | PR-2.2b |
| 相关性阈值 / 去重 | PR-2.2b / PR-4.1 |
| 隐私、DB/socket 权限、日志脱敏 | PR-3.1 / PR-4.3 |
| session fallback / `turnIndex` 唯一性 | PR-3.3 / 决策项 |

### 5.4 估算

计划原估 6–8 周偏乐观，修订版写 8–11 周。按本次新增项（中文前移、clear/migration、模型完整性、consolidation 触发、tokenization 修复）和 spike 风险，**更现实的区间是 8–10 周**（单人、顺序执行、含 review 缓冲）；如果 S1（FTS/DDL/`search_text`/多实例 lease）不通过，M2 需要缩范围或重新设计索引，时间另计。

### 5.5 第三轮审阅后的语义修订

第三轮意见指出的 4 点已并入计划，属于“设计语义闭合”，不扩大范围：

1. **Partial claim**：`claim_working_lease` 返回实际 claim 的集合；只对该集合生成 episodic；`partial` / `remaining_unconsolidated` 让调用方继续处理，不再整体回滚。第四轮进一步改为“claim 在摘要前提交、摘要不持有事务”。见 PR-1.3。
2. **`source_sha` 参与完成态**：相同 hash 幂等短路；不同 hash 视为“更新 turn”，事务内替换 structured 并更新 hash；文本变化时旧 embedding 失效并重算/回填。见 ADR-2 / PR-1.2。
3. **PR-1.6 依赖 PR-1.7**：执行顺序明确为 PR-1.7 → PR-1.6，二者同批发布；PR-1.6 的 Done 条件包含 PR-1.7 已合入。见 5.3。
4. **模型校验和**：不再“算一个 hash 就算校验”，改为版本化 manifest + 固定 upstream revision + 文件 `size`/`sha256` 比对；自定义模型需自带 manifest；`VERIFY=0` 仅开发/本地构建可用，且仍校验结构/维度/tokenizer。见 PR-1.4。

修订后，计划可以进入 issue 拆分。

### 5.6 第四轮审阅后的边界修订

第四轮意见指出的是“跨事务步骤被写成了单事务承诺”，不是新的功能范围。计划已按以下边界修订：

1. **Consolidation 的 LLM 摘要不在数据库事务内**：新增 `claimConsolidation` / `heartbeatConsolidation` / `commitConsolidation` RPC；先短事务 claim lease，TS 在事务外生成摘要并心跳续期，再用短事务校验 owner/内容版本并插入 episodic；`summary_of` 必须与 claim 集合严格相等；lease 过期可恢复，原 owner 提交会被拒绝。见 PR-1.3。
2. **FTS 迁移有单 leader lease**：`fts_migration_lock` 只有 owner 执行回填/建索引；读者按 `fts_state` 查询，不等待锁；状态切换前必须满足“回填完成 + v2 探测通过”，并有双实例并发/崩溃测试。见 8.1。
3. **clear/migrate 区分 DB 事务与索引重建**：DB 删除/导入可回滚；FTS/向量重建失败不回滚已提交数据，返回 `index_state=partial` + `needs_rehydrate`/`fts_dirty`，由幂等 rebuild 收敛。见 ADR-2 / PR-2.6。
4. **`PI_MEMORY_MODEL_VERIFY=0` 只用于开发/本地构建**：release 下禁用并拒绝该配置；即使关闭 hash 校验，仍检查文件结构、ONNX 加载、tokenizer、输出维度和模型兼容性。见 PR-1.4。

因此本文第 5.5 节的“可以进入 issue 拆分”应理解为：**第三轮语义问题已闭合；第四轮事务/迁移边界、第五至七轮的派生数据失效与恢复状态补齐后，才适合按 PR-1.2 / PR-1.3 / PR-2.6 / 8.1 拆 issue。**

### 5.7 第五轮边界补充

第五轮审阅后，计划进一步明确以下并发与发布语义（当前实施编号均以 `REPAIR_PLAN.md` 5.1 速查表为准）：

1. **Retain 快照冲突**：事务外提取后，在写事务内复核 source snapshot；不一致时 rollback 并返回 `snapshot_changed`，有限重试后仍冲突则明确返回可重试错误，不允许旧提取结果覆盖新文本。测试见计划 #59。
2. **FTS migration fencing**：lease 除 owner/expiry 外使用单调递增 fencing token；token 校验与每批回填写入/状态切换处于同一事务（`CREATE INDEX IF NOT EXISTS` 可重复执行，不要求与锁同一事务），旧 leader 恢复后不能写入或切换状态。测试见计划 #60。
3. **S1 失败时的发布门禁**：PR-1.7 与依赖它的 PR-1.6 不可拆开发布；替代 FTS 方案通过 CJK、Top-K、性能门槛后才同批进入 v0.1.17，否则二者都延期并明确中文召回仍是已知 P0。
4. **clear 的崩溃恢复**：删除事务同时持久化 `fts_dirty` / `needs_rehydrate`；启动时识别标志并禁止信任旧向量索引，重建成功后才清除标志。测试见计划 #61。

该补充仅闭合旧提取结果覆盖、过期迁移 leader 写入和 DB/索引提交间崩溃窗口，不改变既定产品范围。

### 5.8 第六轮兼容性修订

旧 server 在 v2 FTS 状态下仍可写 `content`，但不会填充新增的 `search_text` 列；若恢复新版后直接按 v2 查询，会漏掉回滚期间新增的行。计划现明确：

- 版本切换时不得让新旧 server 同时写同一 DB；旧 server 停止后，旧版可在保留 v1 索引时读写 DB。
- 新版恢复时，在开放 recall 前检查 v2 状态下的 NULL `search_text`；发现 NULL 即持久化回退至 v1 `content` 路径，并置 `fts_dirty`，保证先可召回。
- 迁移 leader 随后补齐 NULL、验证 v2 索引，再切回 `search_text`；未完成前不能只查 v2。
- 新增测试覆盖“旧版写入 → 新版启动回退 v1 → 回填 → 切回 v2”的完整恢复链路（计划测试 #62）。

因此对旧 server 的兼容承诺包含“可读写 + 新版恢复时修复 FTS”，不再假设旧 server 会维护 v2 专用列。

### 5.9 第七轮派生数据与恢复状态修订

第七轮复核发现的问题不是新功能，而是“派生数据失效”和“崩溃窗口”仍缺明确合同：

1. **Retain 快照冲突不能复用旧 LLM 结果**：`snapshot_changed` 后 Rust 必须返回当前 `source_sha`，由调用方基于最新 working 文本重新调用 LLM；规则路径可重新提取。旧 pending 文本不得覆盖较新的 working。见 PR-1.2。
2. **已 consolidated working 更新会使旧 episodic 失效**：旧 episodic 设置 `valid_until`，其 `summary_of` 覆盖的整组 working 清空 `consolidated_at`/lease，下一次 consolidation 重算；超限时置 `consolidation_dirty` 并后台处理。见 PR-1.2 / PR-1.3 / PR-3.5。
3. **migrate 崩溃窗口**：导入事务内持久化 `index_state=partial`/`fts_dirty`/`needs_rehydrate`，重建成功后才清除。见 PR-2.6。
4. **FTS fencing 不要求 DDL 与锁同一事务**：回填写入与状态切换必须 fencing；`CREATE INDEX IF NOT EXISTS` 可重复执行，旧 leader 可建同名索引，但不能改 `fts_phase`/`fts_state`。见 8.1。
5. 修正 PR-2.1 的 `drop_fts()` 旧描述，使其与 8.1 的 v1/v2 并存策略一致。

对应测试为计划 #63–#66；拆 issue 时把 #63 作为 PR-1.2/PR-1.3 的共同 Done 条件，避免“working 已更新但旧 episodic 仍在召回”的回归。



---

## 6. 建议的执行顺序

### v0.1.17：止血（约 2.5–3 周）

目标：**写入不丢、重试不重复、中文能搜、FTS 为空时向量仍能召回、episodic 有向量。**

1. PR-1.1 内容寻址 ID
2. PR-1.2 retain 事务 + 完成标记
3. PR-1.3 consolidation claim lease + 事务外摘要 + 稳定 ID
4. PR-1.4 episodic 向量 + 回填 + 模型过滤 + 模型完整性 + tokenization 修复
5. PR-1.5 结构化时间戳
6. PR-1.7 `search_text` + CJK 查询 + FTS Top-K（先行）
7. PR-1.6 独立向量 Top-K（依赖 PR-1.7，同批发布）

发布前必须有的红转绿测试：3 条 fact 落 3 行、retry 幂等、consolidate 两次只 1 条、episodic 有 embedding、FTS 为空时 vector 返回 K 条、“向量”命中中文、1970 不再出现。

### v0.2.0：召回质量（约 2–3 周）

1. PR-2.1 FTS 真实数据复核 + `ngram` spike + legacy scope 慢路径
2. PR-2.2b 完整 RRF + 阈值 + HashMap 聚合
3. PR-2.3 结构化词项召回（含 10k P95 门槛）
4. PR-2.4 元数据生效 + consolidated working 排除/去重 + 模型过滤
5. PR-2.5 多语言 embedding 评估（条件性）
6. PR-2.6 clear 语义 + 迁移 scope 安全

保留 `PI_MEMORY_RECALL_MODE=legacy` 一个版本。

### v0.2.x / v0.3：可靠性与成本（约 3–4 周）

1. PR-3.1 per-project lock + socket 生命周期 + poison/conn_count + 权限
2. PR-3.2 超时分级 + 不销毁连接 + job/status
3. PR-3.3 outbox + user prompt 捕获 + session fallback
4. PR-3.4 compaction hooks
5. PR-3.5 consolidation 自动触发
6. PR-4.x 评估、token 预算、文档

**如果只能做三件事**：PR-1.1、PR-1.2、PR-1.3。它们保证“写入不丢、重试不重复”；紧接着 PR-1.4（episodic 向量）和 PR-1.7/1.6（中文 + 独立向量），才谈得上“记忆能被搜到”。

---

## 7. 需要产品确认的问题

1. **中文是否一等场景？** 如果是（默认按“是”处理），PR-1.7 必须在 v0.1.17；如果不是，中文可以降级为 P1，但仍需修“整句 phrase”这个明显 bug。
2. **`turn_end` / `turnIndex` 在 subagent 之间是否唯一？** 当前 session 缺失时回退 `"default-session"`，多 agent 并行可能共享 ID 空间；需要确认 Pi 的 session 语义，或改成 `(session_id, turn_id)`。
3. **每轮都调用 LLM 提取的成本是否可接受？** outbox 只是把成本移到后台，不减少调用次数；需要确认是否允许“规则优先、LLM 抽样/按重要性升级”。
4. **episodic 应该在什么时候产生？** 如果 consolidation 改为自动，频率/阈值是什么（每 N 轮、每次 compact、还是每日）？用户是否需要“不要总结这一轮”的控制？
5. **旧数据中已丢失的多条 fact 是否要恢复？** 旧版覆盖后无法从 DB 恢复；只能 release notes 说明 + 可选 `/memory reextract`。
6. **Windows 是否正式支持？** 短期建议 socket 模式禁用、回退 stdio，并在 README 标注 experimental。
7. **隐私边界**：记忆内容是否允许进入 debug log？多用户共享机器时，DB/socket 权限和清理策略是什么？

---

## 附录：证据索引

| 主题 | 位置 |
|---|---|
| 固定结构化 ID / 覆盖 | `crates/core/src/runtime.rs:143-187` |
| retain 早退 / 非事务 | `crates/core/src/runtime.rs:101-110` |
| consolidation 非事务、无向量 | `crates/core/src/runtime.rs:233-292` |
| CJK 整句 phrase | `crates/core/src/runtime.rs:381-392` |
| FTS 无 LIMIT + scope 谓词 | `crates/core/src/storage/turso.rs:497-518` |
| structured 整句 LIKE | `crates/core/src/runtime.rs:433-482` |
| 向量 allowlist / `max(1)` | `crates/core/src/embedding_runtime.rs:341-376` |
| 先删后加 / 批量孤儿 | `crates/core/src/embedding_runtime.rs:278-338` |
| 同步加载模型 / Failed 重试 | `crates/core/src/embedding_runtime.rs:119-156` |
| special tokens / padding | `crates/core/src/embedding.rs:58-81` |
| 模型只检查存在 | `crates/core/src/model_download.rs:22,76-90` |
| clear 清全局表 | `crates/core/src/storage/turso.rs:980-995` |
| 迁移不过滤 scope / 改写源库 | `crates/core/src/storage/turso.rs:298-356` |
| 1970 时间戳 | `crates/core/src/core/extraction.rs:9,58,74,91,114` |
| turn_end 只 assistant / 60s | `ts/index.ts:53,172-189` |
| consolidation 仅 quit+confirm | `ts/index.ts:59-71,125-147` |
| 2s 超时销毁连接 | `ts/src/client.ts:22-23,376-394` |
| 全局 Mutex / conn_count | `crates/core/src/bin/pi-memory-server.rs:135-140,175-181,247-251` |
| O(n²) 合并 | `crates/core/src/core/recall.rs:80-95` |
| session fallback | `ts/index.ts:226-230` |
