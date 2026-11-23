# pi-memory-rust 修复 / 优化 / 重构计划

> 基线：`ea66107`（v0.1.16，2026-10-28）
> 依据：外部审阅报告 + [`docs/REVIEW.md`](./REVIEW.md)（详细审阅、证据索引与产品决策）+ 本次对 `runtime.rs` / `turso.rs` / `embedding_runtime.rs` / `embedding.rs` / `model_download.rs` / `pi-memory-server.rs` / `client.ts` / `index.ts` 及 Turso 0.7.2 源码的静态复核。
> 状态：草案 v7（第七轮审阅后修订；Retain 快照、已 consolidated working 更新、migrate 崩溃恢复与 FTS fencing 边界已补齐，可进入 issue 拆分）。
> 说明：本文中的“P0/P1/P2”是**实施优先级**，不是审阅报告里的原始编号；两者存在交叉。问题描述、证据和产品决策以 `REVIEW.md` 为准，本文只保留可执行方案，避免重复。
> **PR 编号以 5.1 的速查表为准。** 1.2 / 1.3 保留的是复核过程记录，其中出现的旧编号（如“PR-2.1 修中文 FTS”“PR-2.2a”）只表示当时的问题归类，不表示当前执行归属。

---

## 0. 执行摘要

这份计划的核心判断只有一句：**先把“写了但丢了、写了但搜不到”修好，再谈召回质量和成本。**

审阅后对 v1 做了三处排期调整（详见 1.4），在第三轮审阅后补齐 3 个语义问题和模型 manifest（详见 1.5），并在第四轮审阅后补齐 consolidation/FTS 迁移的事务边界与 `partial` 失败状态（详见 1.6）：

1. **中文 `search_text`（PR-1.7，原 PR-2.1 最小版）前移到 M1**：中文 FTS 不是质量优化，而是当前核心功能不可用。
2. **独立向量 Top-K（PR-1.6，原 PR-2.2a）前移到 M1**：FTS 为空时只返回 1 条向量，必须先修；**执行顺序为 PR-1.7 → PR-1.6，二者同批发布**；完整 RRF 留在 M2。
3. **`/memory clear <target>`（PR-2.6）和 consolidated working 去重（PR-2.4）前移**：前者是破坏性数据丢失，后者会随自动 consolidation 放大重复注入。

建议的顺序：

1. **第 1–3 周（M1，v0.1.17 止血）**：修结构化 ID 冲突、Retain 半失败、Consolidation 非原子；让 Episodic 真正写入向量并支持缺失回填；修中文 `search_text`、FTS Top-K、独立向量 Top-K；修 1970 时间戳、模型完整性、tokenization。
2. **第 4–6 周（M2，v0.2.0 召回）**：完整 RRF、结构化词项召回、元数据生效、consolidated working 去重、clear/迁移 scope 安全；条件性评估多语言模型。
3. **第 7–8 周（M3）**：server 并发/socket/poison、RPC 超时、outbox、compaction、consolidation 自动触发。
4. **第 9–10 周（M4）**：Token 预算、评估指标、清理、隐私与文档。

具体日期以 spike 结果为准；M1 是唯一建议“先发版止血”的里程碑。单人顺序执行约 **8–10 周**（含 review/未知项缓冲），比 v1 的 6–8 周更保守。

**不建议现在做的事**：换存储/向量引擎、继续调 Top-6、先换 embedding 模型、先做 UI/命令层重构。这些在数据正确性和召回覆盖修好之前，收益都会被上面的问题掩盖。

---

## 1. 复核结论与新增风险

### 1.1 对原报告的处理

| 原报告判断 | 本计划处理 |
|---|---|
| 语义检索受 FTS 候选集限制（P0） | 保留，升级为“双路独立 Top-K + RRF”；同时修 FTS 查询形状，见 2.2 / 2.3 |
| Consolidation 缺少原子提交（P0） | 保留，改为“claim lease + 事务外摘要 + 短事务提交”；稳定 consolidation id，`summary_of` 精确绑定 claim 集合 |
| RPC 超时会终止共享连接或进程（P1） | 保留，并补充“默认 2s 超时 + 同步加载模型”这一更常见的触发路径 |
| 每个 turn 都做 LLM 提取（P1） | 保留，改为“持久化 outbox + 后台 drain”，`turn_end` 只做入队 |
| 记忆来源/冲突/过期字段是死字段（P1） | 保留，先让 working/episodic 的 `valid_until` / `superseded_by` 与 structured 的 `active` / `confidence` 生效，不新增重复字段 |
| 多 Session/Subagent 隔离已过时（已实现单例） | 确认；本计划不再排期“单 Rust Memory Service”，改为修并发、锁粒度和 socket 生命周期 |
| 固定 Top-6（P2） | 保留到 M4；先保证候选集正确，再做 token-aware |
| Compaction 无协同（P1） | 保留到 M3，使用 Pi 的 `session_before_compact` / `session_compact` |
| 质量回归测试不足（P2） | 提前到 M0/M1：每个 P0 修复必须先有失败测试 |
| README 的 FTS blind-spot 测试不存在 | 确认；M4 修文档，并把真实评估脚本落库 |

### 1.2 本次复核新增的 5 个风险点

这 5 点会改变实施顺序，建议在开工前先确认。**以下编号是复核时的归类：风险 A 的落地 PR 是 PR-1.7（验证与收尾在 PR-2.1），不是“PR-2.1 才开始修”。**

#### 风险 A：FTS 查询形状导致 `ORDER BY/LIMIT` 无法下推

当前 SQL：

```sql
SELECT ..., fts_score(content, ?1) AS score
FROM working_memory
WHERE fts_match(content, ?1) AND project_scope = ?2
ORDER BY score DESC
```

Turso 的 FTS 索引只认几种固定模式。`project_scope` 这个额外谓词会让 `WHERE` 无法被索引模式完全覆盖，而 Turso 的优化器在“WHERE 未完全覆盖”时会跳过带 `ORDER BY` / `LIMIT` 的模式（见 Turso `translate/optimizer/mod.rs:400-407`）。按这段逻辑，当前查询会落到不带 `ORDER BY/LIMIT` 的 combined 模式，**一次性取回全部命中再排序**（`MAX_NO_LIMIT_RESULT`），既没有 Top-K，也没有真正的分页。本次用 Turso 0.7.2 做了一个最小 probe：无额外谓词时 `EXPLAIN QUERY PLAN` 只有 `QUERY INDEX METHOD fts`；加上 `AND content = 'x'` 后多出 `USE SORTER FOR ORDER BY`，说明排序/limit 没有下推。PR-1.7 实现时用 10k 行复核，PR-2.1 再做真实数据收尾。

影响：

- 只在 SQL 末尾加 `LIMIT` 不一定能修好；
- `fts_match` 的 standalone 路径使用的是 `SimpleTokenizer`，不是索引配置的 tokenizer；因此只给索引加 `WITH (tokenizer='ngram')` 也可能不生效；
- 10k 规模的 P95 会被常见词查询拖垮。

处理：PR-1.7 重写查询形状 + 引入 `search_text`（见 3.3），并在 PR-1.7 加一个“10k 行、常见词、只返回 K 条”的回归测试；PR-2.1 只做真实数据复核、`ngram` spike 和 legacy 慢路径收尾。

#### 风险 B：默认 2s RPC 超时 + 请求路径内同步加载 ONNX

`client.ts` 的默认超时是 2000ms（`ts/src/client.ts:22-23`），而 `SharedEmbedder::ensure_loaded()` 在状态为 `Unloaded/Failed` 时会**在请求线程里同步加载模型**（`crates/core/src/embedding_runtime.rs:119-156`）。首次 `buildRecallInjection` / `retainTurnMemories` 很可能超过 2s，然后客户端 `conn.destroy()`（`ts/src/client.ts:382-394`），同一客户端的其他 pending 请求一起失败。

这比“RPC 超时杀共享 server”更常见，也更隐蔽：用户看到的是“记忆偶尔没写进去/召回为空”，而不是明显报错。

处理：

- 请求路径绝不加载模型；只使用 `Ready` 状态，未就绪就降级 FTS；
- 模型加载只走后台 warmup / 显式 `prepareModel`；
- 按方法设置超时，超时不销毁共享连接。

#### 风险 C：`turn_end` 只保留了 assistant 输出，没有保留 user prompt

`ts/index.ts:172-177` 用 `extractText(event.message)` 作为 retain 文本；`event.message` 是 assistant 消息。用户在这一轮说的“以后都用中文/不要改这个文件”不一定出现在 assistant 输出里，因此 LLM 提取和规则提取都可能漏掉真正的偏好/指令。

处理：在 `before_agent_start` 记录当前 session 的 user prompt，`turn_end` 用“user prompt + assistant text”作为 retain 文本；跳过 aborted/过短/纯工具结果的 turn。这个改动会直接提升结构化记忆的召回率。

#### 风险 D：socket 模式下 `shutdown` 是全局退出信号

`client.shutdown()` 在 socket 模式也会发送 `shutdown`（`ts/src/client.ts:148-165`），server 收到后设置全局 `exit_flag`（`crates/core/src/bin/pi-memory-server.rs:247-259`）。虽然其他活跃连接还能继续用，但当最后一个连接断开时 server 会退出；下一次请求又要 spawn 新进程。多 session 共享时这是不必要的抖动。

处理：socket 模式只关闭本连接（detach），不发送全局 `shutdown`；server 靠 idle timeout 退出；`shutdown` 只保留给 stdio / 显式管理命令。

#### 风险 E：批量 embedding 会留下孤儿向量

`save_embeddings_batch` 直接 `index.add(embeddings)`，没有先移除同一 `(kind, id)` 的旧 vector id（`crates/core/src/embedding_runtime.rs:313-338`）。回填或重复 hydrate 时，同一个记忆会有多个向量 id，旧 id 仍参与搜索，造成重复候选、索引膨胀和“删除后还能召回”。

处理：所有写入路径统一走 `upsert` 语义，且不能“先删后加”。改为：DB 先 upsert → index 先 add 新 id → 更新 mapping → 再删旧 id；add 失败时保留旧 id/mapping，并标记该 project 需要 rehydrate。详见 PR-1.4。

### 1.3 对第二轮审阅意见的确认

> 历史修订记录。下表“本版修订”中的 PR 编号是当时的归属；当前执行编号以 5.1 速查表为准（例如中文 FTS 已归入 PR-1.7，独立向量已归入 PR-1.6）。

| 审阅意见 | 结论 | 本版修订 |
|---|---|---|
| Outbox 缺少跨进程 claim，可能重复处理 | **属实** | ADR-5 / PR-3.3 增加持久化 lease、owner、过期恢复、失败退避；明确 at-least-once 与重复 LLM 调用策略 |
| RRF 的 `scope_boost=0.01` 不是微小 tie-break | **属实** | ADR-3 / 附录 B 移除 `scope_boost`；候选在进入 RRF 前按 scope 过滤；legacy 多 scope 走单独慢路径 |
| 迁移失败“保留旧 FTS”与步骤不匹配 | **属实** | 8.1 改为“先回填、再建 v2 索引、最后切 `fts_state`”；旧 v1 索引保留一个版本；补了 Turso DDL 回滚/双索引 probe |
| 旧 retain 半成品的恢复来源未定义 | **属实** | ADR-2 / PR-1.2 增加 `retain_state` + `extractor`；明确只有 working.content 可重放，旧 LLM-only 结果不能保证恢复 |
| 向量先删后加缺少失败语义 | **属实** | PR-1.4 改为 add-before-delete + DB-first；失败保留旧映射并标记 rehydrate |
| Structured LIKE 成本没有上限 | **属实** | PR-2.3 增加 10k 数据延迟门槛（P95 ≤ 20ms）与 FTS 回退方案，不再用“记录一下”代替验收 |
| 超时不销毁连接，但服务端仍在执行 | **属实** | PR-3.2 明确“没有取消协议”：超时只放弃响应，写请求必须幂等；长操作改为 job/status，不靠超时释放容量 |
| 估算偏乐观 | **属实** | 5.1 改为粗略区间 + 放行 spike；M1/M2 估算上调，spike 不通过则缩范围 |

本次新增的 Turso DDL probe（临时文件已删除）：在 0.7.2 上执行 `BEGIN; DROP INDEX; CREATE INDEX（故意失败）; ROLLBACK` 后，旧 FTS 索引仍可查询；`DROP + CREATE + COMMIT` 后新索引可查询；同一表上两个 FTS 索引（`content` / `search_text`）可并存并分别命中。因此 8.1 的 v1/v2 并存切换有实现依据，但仍需在 PR-2.1 用真实 10k 数据复核。

### 1.4 与 `REVIEW.md` 的对账（本版调整）

`REVIEW.md` 是完整审阅，本文只保留可执行部分。两边的对应关系：

| `REVIEW.md` 结论 | 本计划处理 |
|---|---|
| 固定结构化 ID 覆盖（P0-1） | PR-1.1，不变 |
| retain 非事务 + working 早退（P0-2） | PR-1.2，不变 |
| 中文 FTS 整句 phrase、无 LIMIT（P0-3） | **PR-1.7（原 PR-2.1 最小版）前移到 M1**；structured LIKE 仍在 M2（PR-2.3） |
| 向量被 FTS allowlist 限制、episodic 无向量（P0-4） | **PR-1.6（原 PR-2.2a）前移到 M1**；完整 RRF 为 PR-2.2b（M2）；episodic 向量仍 PR-1.4 |
| `turn_end` 只保留 assistant、同步 60s（P1-1） | PR-3.3，不变 |
| consolidation 仅 quit+confirm（P1-2） | **新增 PR-3.5**，不再依赖用户确认 |
| `clear <target>` 清全局表（P1-3） | **新增 PR-2.6，从 M4 前移** |
| 迁移不按 scope 过滤（P1-4） | PR-2.6（迁移 scope + 源库只读 + 计数校验）；PR-4.3 只做发布说明 |
| 结构化时间戳 1970（P1-5） | **新增 PR-1.5** |
| 全局 Mutex / 2s 超时 / 同步加载模型（P1-6） | PR-1.4（请求路径不加载）、PR-3.1（per-project lock + poison/conn_count）、PR-3.2（超时分级） |
| 模型只检查存在 + Failed 重试循环（P1-7） | PR-1.4 增加版本化 manifest、pinned revision、退避、后台重试 |
| special tokens / padding / truncation（P1-8） | PR-1.4 修 batch 形状；PR-2.5 评估模型时统一 tokenization |
| consolidated working 仍召回（P2） | **从 PR-4.1 前移到 PR-2.4** |
| RRF 线性合并 O(n²)（P2） | PR-2.2b 改用 HashMap 聚合 |
| 相关性阈值 / 去重（P2） | PR-2.2b（阈值）+ PR-4.1（去重） |
| `content_map` 无界增长、隐私/权限（P2） | PR-3.1 / PR-4.3 |
| session fallback / `turnIndex` 唯一性（P2） | PR-3.3 + 第 11 节决策项 |

**明确不做的调整**：不把整个 M2 前移，也不在 M1 引入完整 RRF。M1 只修“能不能搜到”和“写不写得进去”，排序质量留到 M2，避免止血版引入新的排序回归。

### 1.5 第三轮审阅修订（语义/依赖/完整性）

第三轮意见指出的 3 个语义问题和 1 个模型完整性问题均已确认，处理如下：

| 第三轮意见 | 本版修订 |
|---|---|
| 部分 claim 冲突时整体回滚，会让未处理部分停滞 | **PR-1.3 改为“先 claim lease，只对实际 claim 的集合生成摘要并提交”**（第四轮进一步改为摘要不持有事务）；返回 `partial` / `remaining_unconsolidated`，调用方重新读取并再跑；不再用“claim 数 != 输入数 → 整体回滚”；增加部分重叠的并发测试 |
| `source_sha` 未参与 Retain 完成态判断 | **ADR-2 / PR-1.2 明确 hash 比较**：相同 hash 幂等短路；不同 hash 视为“更新 turn”，事务内替换 structured 并更新 hash；文本变化时删除旧 embedding，commit 后重算/回填 |
| PR-1.6 依赖 PR-1.7，但顺序相反 | **明确执行顺序 PR-1.7 → PR-1.6，二者同批进入 v0.1.17**；PR-1.6 的 Done 条件包含 PR-1.7 已合入，且测试在新 FTS 路径上运行 |
| “校验和”未说明与谁比较 | **PR-1.4 改为版本化 manifest + 固定 upstream revision**：文件 `size`/`sha256` 与 manifest 比对，manifest 像 lockfile 一样 review；自定义模型需自带 manifest；`VERIFY=0` 仅开发/本地构建可用 |

这四点属于“设计语义未闭合”，不是范围扩张；修订后计划可以进入 issue 拆分。实现时仍以对应 PR 的测试为准。

### 1.6 第四轮审阅修订（事务边界、迁移并发、失败状态）

第四轮意见指出的 5 个问题均已确认，处理如下：

| 第四轮意见 | 本版修订 |
|---|---|
| Consolidation 未说明 LLM 摘要是否在数据库事务内；claim 与 insert 之间可能持有写锁数十秒 | **PR-1.3 改为“先持久化 claim lease，再在事务外生成摘要，最后在短事务内校验 lease/内容版本并提交”**；LLM/规则摘要全程不持有数据库事务；补充 claim 过期、心跳、崩溃恢复和并发测试 |
| FTS 迁移在多实例同时启动时可能并发回填/建索引/读到切换中间态 | **8.1 增加 `fts_migration_lock` lease + owner + heartbeat + phase**；只有 leader 执行回填/建索引，读者始终按 `fts_state` 查询，状态切换前验证“回填完成 + v2 索引可用”，补充双实例并发和切换中崩溃测试 |
| “迁移失败回滚”的承诺超过实际原子性 | **ADR-2 / PR-2.6 区分 DB 事务与事务外索引重建**：导入/删除在单事务内可回滚；FTS/向量重建失败不回滚已提交数据，而是返回 `index_state=partial` + `needs_rehydrate`/`fts_dirty`，由幂等 rebuild 收敛 |
| `PI_MEMORY_MODEL_VERIFY=0` 允许任意模型文件进入加载路径 | **PR-1.4 收紧为仅开发/本地场景可用**（release 构建下禁用并拒绝）；即使关闭 hash 校验，仍强制文件结构、ONNX 加载、tokenizer、输出维度和模型兼容性检查 |
| 旧段落的 PR 编号、M1/M2 归属、REVIEW 引用与 v3 拆分不一致 | **5.1 增加 PR 编号速查表**；1.2 / 1.3 标注为历史记录；统一 `search_text` 为 PR-1.7、独立向量为 PR-1.6、clear/迁移为 PR-2.6；同步修订 REVIEW.md |

修订原则：**数据库事务只覆盖“必须一起成功”的 DB 写入；LLM、embedding、FTS/向量重建都放在事务外，并用持久化 lease + 版本校验 + 可重入状态机保证最终一致。** 因此不再使用“整体回滚”描述跨事务步骤，而是明确“DB 已提交、索引待修复”的可恢复状态。

### 1.7 第七轮审阅修订（派生数据与恢复状态）

第七轮复核又发现 4 个会影响正确性的边界，已并入计划：

| 第七轮意见 | 本版修订 |
|---|---|
| `snapshot_changed` 后仍可能复用旧 LLM 结果，且 outbox 只重读 pending 文本无法解决 working 已被更新的情况 | **PR-1.2 明确“Rust 不用旧 `llm_result` 重试”**：返回当前 `source_sha`，由调用方重新读取最新 working 并重跑 LLM；只有规则路径可本地重提取；pending 旧文本不得覆盖较新的 working；增加 stale 结果不落库测试 |
| 已 consolidated 的 working 文本更新后，旧 episodic 仍引用旧内容，新 working 又因 `consolidated_at` 非空永不再摘要 | **PR-1.2 增加“consolidated working 更新”事务**：旧 episodic 置 `valid_until=now`，其 `summary_of` 覆盖的整组 working 清空 `consolidated_at`/lease，下一次 consolidation 重新生成；超限时持久化 `consolidation_dirty` 并后台处理 |
| migrate 导入事务提交后、索引重建前崩溃，没有持久化 dirty 状态，重启可能误信空索引 | **PR-2.6 在导入事务内写 `index_state=partial`/`fts_dirty`/`needs_rehydrate`**，重建成功后才清除；增加崩溃恢复测试 |
| FTS fencing 被写成“建索引也必须同一事务”，但 DDL 不一定能与锁更新同事务 | **8.1 改为“回填写入和状态切换必须 fencing；建索引可重复执行”**：旧 leader 可建同名索引，但不能改 `fts_phase`/`fts_state`；测试改为“写入/phase/切换被拒绝” |

同时修正了 PR-2.1 中“版本变化直接 `drop_fts()`”与 8.1 v1/v2 并存策略的冲突，以及测试 #58 的命名。



---

## 2. 目标与非目标

### 2.1 目标（可验收）

| 维度 | 目标 | 验收方式 |
|---|---|---|
| 数据正确性 | 同一 turn 的 N 条 fact/preference/instruction/timeline 全部落库；重试不重复、不丢失 | 单元/集成测试：3 条 fact → 3 行；重试 → 仍 3 行 |
| 事务性 | Retain / Consolidation 的 DB 写入要么全成功、要么全回滚；LLM 摘要不持有写事务；无“working 已写、structured 永久缺失” | 故障注入 + 重试 + “LLM 调用期间无打开事务”断言 |
| 语义召回 | Episodic 写入向量；缺失向量可回填；向量 Top-K 不受 FTS 候选集限制 | 改写 query 能召回 episodic；FTS 为空时 vector 仍返回 K 条 |
| 中文召回 | “向量”能命中“项目使用 turbovec 向量索引”；长中文 query 能按词项召回 | CJK 子串测试 + 长 query 测试 |
| 结构化召回 | 正常长 prompt 能命中 facts/preferences/instructions，而不是只匹配整句 | “怎样避免…回复使用中文”命中“回复使用中文” |
| 时间正确性 | 新写入的结构化记忆使用真实 turn 时间，不再出现 1970 | 规则/LLM 两条路径的时间戳测试 |
| 破坏性操作安全 | `clear <target>` 只影响目标 scope；迁移不跨 scope 混入；命令明确返回 DB 已提交与索引待重建状态 | 双项目 clear/迁移测试 + 索引重建失败注入 |
| 模型可用性 | 损坏/半截模型文件不会进入 Ready；失败重试有退避，不阻塞请求；生产环境不能关闭完整性校验 | manifest 校验测试 + `VERIFY=0` 构建矩阵 + 请求路径无加载 |
| 可靠性 | 单请求超时不销毁共享连接；两个 session 不互相阻塞；socket 冷启动只有一个 server | 并发/冷启动测试 |
| 成本 | `turn_end` 的同步路径 < 100ms；LLM 调用在后台；可观测队列深度 | 生命周期测试 + stats |
| 质量 | 有可重复的 Recall@5/10、MRR、nDCG、P50/P95 报告 | `pi-memory-eval` + CI/手动基准 |

### 2.2 非目标（本计划不做）

- 不更换 Turso / turbovec / ort；
- 不重写 JSON-RPC 协议（只加字段/方法，保持向后兼容）；
- 不在 M1 之前换 embedding 模型；
- 不先做“记忆编辑 UI”“记忆图谱”“自动遗忘策略”这类产品功能；
- 不追求与 TS reference 的 bit-exact 行为；从 RRF 开始，行为等价性让位于可测的召回质量（见 8.4）。

---

## 3. 目标架构

### 3.1 三条主路径

```
turn_end
  └─ enqueueTurn (fast, durable outbox)
        └─ worker: LLM extract → retainTurnMemories
              ├─ tx: working + facts/preferences/instructions/timeline + session_state
              └─ post-commit: embedding (best effort) → backfill if unavailable

consolidate
  ├─ tx: claim working lease (commit)
  ├─ outside tx: LLM/rule summary over claimed set (+ heartbeat)
  ├─ tx: verify lease/version + insert episodic (summary_of = claimed set) + mark consolidated
  └─ post-commit: episodic embedding (best effort) → backfill

recall
  ├─ analyze query → terms + search_text query
  ├─ FTS Top-K (BM25, LIMIT)
  ├─ Vector Top-K (global, no FTS allowlist)
  ├─ Structured Top-K (terms + validity/active/confidence)
  └─ RRF merge → eligibility/dedup → token budget → render
```

### 3.2 新增/调整的存储对象

| 对象 | 变化 | 目的 |
|---|---|---|
| `working_memory.search_text` | 新增列，写入时生成 CJK bigram + 英文词 | 让 CJK 子串和长 query 可检索，不依赖 Turso tokenizer 细节 |
| `episodic_memory.search_text` | 同上 | 同上 |
| `fts_working_idx` / `fts_episodes_idx` | 改为索引 `search_text` | 与查询词项一致 |
| `pending_turns` | 新增表 | `turn_end` 持久化队列，进程退出不丢 |
| `app_meta`（或 `PRAGMA user_version`） | 新增 | schema/FTS tokenizer/model 版本迁移；也持久化 `consolidation_dirty` 等恢复标记 |
| `fts_migration_lock` | 新增 | FTS 迁移单 leader lease/heartbeat |
| `memory_embeddings.model` | 真正参与 hydrate 过滤 | 换模型后不混入旧向量 |
| `working_memory.metadata_json` | 增加 `retain: "complete"` | 区分“working 已写”和“turn 已完成” |
| `consolidated_at` | 作为“已提交”标记 | 防止重复 consolidation |
| `working_memory.consolidation_lease_*` | 新增 | 允许摘要生成在事务外进行，并支持崩溃恢复 |

### 3.3 为什么用 `search_text`，而不是只加 `ngram`

Turso 0.7.2 确实支持 `WITH (tokenizer='ngram')`（`turso_core-0.7.2/index_method/fts.rs:1478`），但它有两个不确定性：

1. `fts_match` 的 standalone 实现使用 `SimpleTokenizer`（`fts.rs:159-190`），是否走到索引 tokenizer 取决于执行计划；
2. `ngram` 的 2–3 字符切分对长中文 query 仍然会产生大量 phrase 约束，查询构造稍有不慎就退化成“整句精确匹配”。

更可控的方案是**在写入时物化一份 `search_text`**：

```
原文：项目使用 turbovec 向量索引
search_text：项目 目使 使用 turbovec 向量 量索 索引
```

查询侧用同一个 `build_search_text` 生成词项，再拼 `"term1" OR "term2"`。这样：

- 不依赖 Turso 的 tokenizer 是否在查询侧生效；
- 即使查询退回 `fts_match` 的 standalone 路径，`search_text` 已经是空格分隔的 token，仍然能命中（只是性能退化，不会像 `ngram` 那样“索引是 ngram、查询是 SimpleTokenizer”而完全失效）；
- CJK 子串天然可命中；
- 未来换 jieba/其他分词器只改 `build_search_text`，不用改 FTS schema；
- 英文仍按词切分，行为可预测。

`ngram` 作为 PR-2.1 的一个 1 天 spike 保留：如果它能稳定地让 FTS 索引模式命中，并且查询侧一致，可以作为“少一列”的替代方案；但默认走 `search_text`。

---

## 4. 关键设计决策

### ADR-1：结构化记忆使用内容寻址 ID，忽略 LLM 给的 ID

**决策**：`fact_id = "{turn_memory_id}:fact:{hash16}"`，preference/instruction/timeline 同理。hash 输入是规范化后的内容字段；LLM 返回的 `factId` 不再使用。

**理由**：

- 同一 turn 多条 fact 必须有唯一 ID；
- 重试同一 turn 时，同样的内容生成同样的 ID，`ON CONFLICT DO UPDATE` 天然幂等；
- 不信任 LLM 生成的 ID（可能重复、为空、跨 turn 冲突）。

**替代方案**：序号 `:fact:0/1/2`。实现更简单，但重试时 LLM 返回顺序变化会产生重复行；内容 hash 更稳。

**注意**：跨 turn 的同一事实仍会是两行；跨 turn 去重放到 M4 的 dedup，不和本修复混在一起。

### ADR-2：事务边界

| 操作 | 事务内（必须一起成功） | 事务外（可重试/可降级） |
|---|---|---|
| Retain | delete 旧 structured（按 `source_memory_id`）+ delete 旧 embedding（`source_sha` 变化时）+ insert working + insert structured + save session_state + 已 consolidated 时让旧 episodic 失效/清 `consolidated_at` | embedding 重算（hash 变化时）、后台重算超限的 consolidation |
| Consolidation | 校验 lease/内容版本 + insert episodic + 标记 `consolidated_at` + 清 lease | claim lease、LLM/规则摘要、episodic embedding |
| Migration | 目标 DB 的批量导入 + 计数校验（同一目标事务） | 源库读取、FTS rebuild、向量 hydrate |
| Clear | 目标 scope 的 DB 删除 | FTS rebuild、向量 rehydrate |

**理由**：embedding 和 LLM 可能慢、可能失败，不能让它持有写锁；DB 是 source of truth，向量/FTS 索引可以从 DB 重建。反过来，working/structured/session_state 必须一起成功，否则会出现“working 已写、structured 永远不补”的状态。

**事务外步骤的失败语义**（第四轮修订）：

- 事务外步骤失败**不回滚已提交的 DB 写入**，也不能在返回值里写成“整体失败/已回滚”。
- 每个项目维护 `index_state`（`ok` / `partial`）与细粒度标记：`fts_dirty`、`needs_rehydrate`。`partial` 时查询仍可用（FTS 走旧索引/降级，向量按 DB 校验或跳过缺失项），并在 `diagnose` 中可见。
- 恢复动作统一为幂等的 `rebuild-fts` / `rebuild-embeddings` / `hydrate`；重复执行不产生重复行或孤儿向量。
- `clear` / `migrate` 的返回值必须同时给出 `db_committed` 与 `index_state`，不能只用一个 `ok/error` 模糊两者。
- 若目标 DB 是本次新建且导入事务失败，可以删除目标文件重来；若目标 DB 原本已有数据，失败时事务回滚后保持原状。这两种情况要在迁移报告里区分。

**Retain 的幂等/状态判断**改为：

| `metadata.retain` | 含义 | 再次调用时 |
|---|---|---|
| 缺失 / `working` | 旧版半成品，只有 working | 仅用 `working.content` 重放**规则提取**；标记 `rule_complete`，不声称恢复 LLM-only 项 |
| `rule_complete` | 规则路径已完成（`extractor=rule`） | `source_sha` 相同且无 `llm_result` → 直接返回；相同且有 `llm_result` → 升级重跑并标 `llm_complete`；`source_sha` 不同 → 视为文本更新，删除旧 structured 后重跑 |
| `llm_complete` | LLM/规则路径均已完成 | `source_sha` 相同 → 直接返回；`source_sha` 不同 → 视为文本更新，事务内替换 structured 并更新 hash |
| `failed` | 上次失败 | 重试 |
| `unrecoverable` | `working.content` 为空/不可重放，无法自动修复 | 只做诊断；若后续拿到文本，允许重新走 `working` |

同时写 `extractor: "llm" | "rule"`、`extracted_at`、`source_sha`（`working.content` 的 SHA-256，按 UTF-8 原始字节计算）。**`source_sha` 是完成态判断的一部分**：相同 turn ID 但文本变了，不是幂等重试，而是“更新这个 turn”。处理规则：

1. 计算新文本的 `source_sha`；与 `working.metadata.source_sha` 相同 → 幂等短路，不重写 structured，也不重算 embedding。
2. 不同 → 在事务内 `delete_structured_by_source_memory` + upsert working（新 content / `source_sha` / `updated_at`）+ 重新插入 structured + 更新完成态。
3. 文本变了但本次没有 `llm_result`：旧 LLM-only 项已失效，只能保证规则路径；状态降为 `rule_complete`，`diagnose` 标 `llm_re_extract_needed`，不保留旧 LLM 结果冒充新文本的提取。
4. 文本变了且本次有 `llm_result`：走 LLM 路径，状态 `llm_complete`。
5. **embedding 时机**：`source_sha` 变化时，在同一事务内删除该 working 的旧 embedding 行（避免旧向量继续对应新文本）；commit 后按新文本重新 embedding。若模型未就绪，保持“缺失”状态，由 PR-1.4 回填，不能保留旧向量。`source_sha` 未变时不重算。

**恢复数据来源**只有 `working_memory.content`（当前确实保存了完整 turn 文本）；旧版没有持久化 LLM payload，因此旧 turn 的 LLM-only fact 不能保证恢复。需要 LLM 重提取时，走显式的 `/memory reextract <turn>`（会再次调用 LLM），不做自动后台重放。内容为空/无法重放的旧行标 `unrecoverable` 并在 `diagnose` 计数，不能标成 complete。

### ADR-3：双路召回 + RRF，不再做“FTS 候选 allowlist”

**决策**：

- FTS 返回 Top-K（默认 50），使用真实 BM25 分数；
- Vector 返回全局 Top-K（默认 50），不传 allowlist；
- Structured 返回 Top-K（默认 30），按词项匹配；
- 三路用 RRF 融合：`score = Σ w_s / (k + rank_s)`，默认 `k=60`，`w_fts=1.0`、`w_vector=1.0`、`w_structured=0.8`；
- **RRF 公式中不含 `scope_boost`**。候选在进入 RRF 之前按 `project_scope` 过滤；DB 按项目物理隔离时这一步是 no-op。若检测到单 DB 多 scope（legacy 迁移数据），直接走 `legacy_scoped` 慢路径（查询里带 scope 条件/后过滤），而不是“先取全局 Top-K 再过滤”，避免其他 scope 挤掉当前 scope 的候选。若确实需要 scope 偏好，只作为最终排序的 tie-break（`rrf_score` 相同后比较 `in_scope`），不得参与分数。

**理由**：

- 当前 allowlist 让向量检索受 FTS 候选集限制；FTS 为空时只取 1 条（`embedding_runtime.rs:354-362`）；
- 当前 `fts_score = 1 - index/(total+1)` 丢弃了 BM25，且第一名恒为 1.0（`runtime.rs:394-431`）；
- RRF 不需要把 BM25 和 cosine 校准到同一量纲，适合异构召回源；
- `scope_boost=0.01` 与 RRF rank 1 的 `1/61≈0.0164` 同量级，会直接改写来源排名；因此必须移除，而不是“调小”。
- 保留 `RecallScoreBreakdown` 的逐源贡献，方便诊断；`scope` 只作为诊断字段，不进入分数。

**替代方案**：BM25 归一化后加权求和。需要按 query 做 max 归一化，长尾不稳定；RRF 更稳。

### ADR-4：Embedding 生命周期与模型版本

**决策**：

- 请求路径只使用 `Ready` 状态的 embedder；`Unloaded/Failed` 直接降级 FTS，不同步加载；
- 启动时后台 warmup；`prepareModel` 显式下载 + 加载；
- `memory_embeddings.model` 写入统一的 model id，`hydrate` 只加载当前 model；
- 新增 `backfillEmbeddings` / `/memory rebuild-embeddings`；
- 换模型时旧向量不混入，按需全量重建。

**理由**：当前 `save_embedding` 在模型未就绪时静默 `Ok(())`，没有回填；`hydrate` 只检查维度 384，不检查模型（`embedding_runtime.rs:230-247`）；`save_embedding_vector` 硬编码 `all-MiniLM-L6-v2`（`embedding_runtime.rs:290-295`）。

**多语言模型**：见 PR-2.5，先评估再切换；切换需要支持 2/3 输入、动态维度、pooling 策略，不在 M1 做。

### ADR-5：`turn_end` 只入队，LLM 在后台 drain

**决策**：新增 `pending_turns` 表。`turn_end` 只做：

1. 取当前 session 的 user prompt（`before_agent_start` 缓存）；
2. `enqueueTurn` 写入 `(session_id, turn_index, project_scope, text)`；
3. 立即返回；后台 worker 做 LLM 提取 + `retainTurnMemories`。

**跨进程 claim/lease（必须）**：`drainPendingTurns` 的“串行”只约束单进程，不能防止多个 session/插件实例/重启交叠同时处理同一 turn。表增加：

```sql
status TEXT NOT NULL DEFAULT 'pending'  -- pending | processing | done | failed
lease_owner TEXT
lease_expires_at TEXT
attempts INTEGER NOT NULL DEFAULT 0
next_attempt_at TEXT
last_error TEXT
updated_at TEXT
```

claim 必须在一个事务里做“条件更新 + 检查 changes”：

```sql
BEGIN;
SELECT session_id, turn_index, text
FROM pending_turns
WHERE (status IN ('pending', 'failed')
       OR (status = 'processing' AND lease_expires_at < ?now))
  AND attempts < ?max_attempts
  AND (next_attempt_at IS NULL OR next_attempt_at <= ?now)
ORDER BY turn_index
LIMIT 1;
UPDATE pending_turns
SET status='processing', lease_owner=?owner, lease_expires_at=?expires, updated_at=?now
WHERE session_id=?sid AND turn_index=?idx
  AND (status IN ('pending', 'failed')
       OR (status = 'processing' AND lease_expires_at < ?now))
  AND attempts < ?max_attempts
  AND (next_attempt_at IS NULL OR next_attempt_at <= ?now);
-- changes() == 1 才算 claim 成功；否则 rollback/重试
COMMIT;
```

- `owner` 用 `{pid}:{session}:{random}`，不依赖进程内单例；
- lease 默认 5 分钟（大于 LLM 60s 超时 + retain 预算）；长任务可加 heartbeat RPC，或把 lease 设为 LLM 超时的 2 倍；
- 成功：`retainTurnMemories` 成功后再 `complete`，且 `WHERE lease_owner=? AND status='processing'`；完成条件必须是“retain 已提交”；
- 失败：释放 lease、`attempts++`、`next_attempt_at` 指数退避；超过 max attempts 标 `failed`，保留 `last_error`；
- 崩溃/超时：lease 过期后可被其他进程 claim；
- **语义是 at-least-once，不是 exactly-once**：lease 过期后可能重复调用 LLM；重复的 retain 由确定性 ID/事务保证幂等，LLM 调用本身不保证只发生一次。这个代价要写进文档，不能假装是 exactly-once。

**理由**：

- 当前 `turn_end` 被 Pi await，LLM 超时 60s（`ts/index.ts:49-53`、`172-189`），每轮最多阻塞 agent 收尾 60s；
- 进程退出时队列不能丢，所以放 DB 而不是纯内存；
- 多 session/重启交叠是正常场景，必须有持久化 claim，不能靠进程内 `draining` 标志；
- 顺带解决“只保留 assistant 文本”的问题：入队时组合 user + assistant。

**替代方案**：纯 TS 内存队列 + shutdown flush。实现快，但崩溃/强退会丢 turn，且无法跨进程 claim；不推荐作为默认。

### ADR-6：Server 并发与 socket 生命周期

**决策**：

- `StoragePool` 改为 `HashMap<String, Arc<Mutex<ProjectRuntime>>>`，只锁项目，不锁全局；
- `prepareModel` / 模型下载不持有项目锁；
- socket 启动使用 lock file + “先 connect 再判断 stale”，不再无条件 `remove_file`；
- socket 权限：目录 `0700`、socket `0600`；
- socket 模式 client 的 `shutdown` 只关闭本连接，不发全局 shutdown；
- Windows 短期禁用 socket 模式，回退 stdio；长期再做 named pipe。

**理由**：当前全局 `Mutex<SharedState>` 让一个慢请求阻塞所有项目（`pi-memory-server.rs:247-251`）；无条件 `remove_file` 再 bind 存在双 server 竞态（`118-129`）；Windows 上 server 回退 stdio，而 client 在 socket 模式不读 stdout。

### ADR-7：先让现有元数据字段生效，不新增重复字段

**决策**：M2 只做：

- `valid_until` / `superseded_by`：**只对 working/episodic** 生效（这两个表本来就有列）；FTS 后过滤，过期记忆由清理任务移除；
- `instructions.active`：structured 查询过滤；
- `facts.confidence`：参与 structured 排序/分数；
- `memory_embeddings.model`：hydrate 过滤。

**理由**：`valid_until` / `superseded_by` 目前只存在于 `working_memory` / `episodic_memory`（`models.rs:56-61`、`79-84`），structured 表没有这两列；先把已有字段用起来，不为了“统一”给四张结构化表再加一遍列。若后续产品确实需要 fact 级过期，再在 PR-2.4 单独加列并迁移。

---

## 5. 里程碑与 PR 拆分

### 5.1 总览

| 里程碑 | 主题 | PR | 粗略净开发估算（不是承诺） | 依赖 |
|---|---|---|---|---|
| M0 | 基线、失败测试、开关 | PR-0.1 | 1–2 天 | 无 |
| M1 | 止血：数据正确性 + 中文 + 独立向量 | PR-1.1 ~ 1.5、PR-1.7（先行）→ PR-1.6（依赖 1.7） | 12–16 天 | M0 + S1 |
| M2 | 召回质量与结构化 | PR-2.1（收尾）、PR-2.2b、PR-2.3 ~ 2.6 | 8–12 天 | M1 + S3 |
| M3 | 可靠性与成本 | PR-3.1 ~ 3.5 | 9–12 天 | M2 + S2 |
| M4 | 质量、评估、维护 | PR-4.1 ~ 4.3 | 5–8 天 | M3（部分可并行） |

单人顺序执行约 **8–10 周**（含 review/未知项缓冲）；M1 约 **2.5–3 周**可独立发布止血。上面是范围，不是排期承诺；任何 spike 失败都会改变后续范围。

**PR 编号速查（当前执行归属）**：

| PR | 里程碑 | 范围 | 备注 |
|---|---|---|---|
| PR-1.1 | M1 | 结构化记忆内容寻址 ID | 忽略 LLM 给的 ID |
| PR-1.2 | M1 | Retain 单事务 + 完成标记 + `source_sha` + 旧 episodic 失效 | 含文本更新替换/快照冲突 |
| PR-1.3 | M1 | Consolidation claim lease + 事务外摘要 + 稳定 ID | LLM 不在事务内 |
| PR-1.4 | M1 | Episodic 向量 + 回填 + 模型 manifest + tokenization | `VERIFY=0` 仅开发 |
| PR-1.5 | M1 | 结构化时间戳/来源字段 | 去掉 1970 |
| PR-1.7 | M1（先行） | `search_text` + CJK 查询 + FTS Top-K | 原 PR-2.1 最小版 |
| PR-1.6 | M1（依赖 1.7） | 独立向量 Top-K | 原 PR-2.2a；与 1.7 同批发布 |
| PR-2.1 | M2 | FTS 真实数据复核、ngram spike、legacy scope 慢路径 | 不再重复实现 1.7 |
| PR-2.2b | M2 | 完整 RRF + 阈值 + HashMap 聚合 | 2.2a 已在 PR-1.6 |
| PR-2.3 | M2 | 结构化按词项召回 + 10k P95 门槛 | 仍走 LIKE/terms |
| PR-2.4 | M2 | 元数据生效 + consolidated 去重 + 模型过滤 | eligibility cache |
| PR-2.5 | M2 | 多语言 embedding 评估 | 条件性 |
| PR-2.6 | M2 | clear scope + 迁移 scope + 索引 partial 状态 | 从 M4 前移 |
| PR-3.1 ~ 3.5 | M3 | server 并发/socket、超时、outbox、compaction、自动 consolidation | 可靠性 |
| PR-4.1 ~ 4.3 | M4 | token 预算、评估、清理/文档 | 质量与维护 |

> 历史段落（1.2 / 1.3）中出现的“PR-2.1 修中文”“PR-2.2a”只表示当时的问题归类；执行以本表为准。

**放行 spike（必须在对应里程碑开始前完成）**：

| Spike | 要回答的问题 | 时间盒 | 不通过时 |
|---|---|---|---|
| S1 FTS/DDL/`search_text` | Turso 0.7.2 上 10k 行能否稳定 `LIMIT` 下推；v1/v2 索引并存/切换是否可靠；多实例 lease 是否可重入；`search_text` 召回是否达标 | 1–2 天 | 先实现并验证替代 FTS 方案；PR-1.7 与依赖它的 PR-1.6 不得单独进入 v0.1.17。替代方案通过 CJK、Top-K、性能门槛后两项同批发布；否则两项都延后并明确该版本仍有中文召回 P0 |
| S2 并发存储接口 | `TursoStorage` / `ProjectVectors` / `Connection` 是否能放进 `Arc<Mutex<ProjectRuntime>>` 并跨线程 | 0.5–1 天 | 退回“每项目一个 worker 线程 + channel”，M3 估算 +2–3 天 |
| S3 structured LIKE 10k | 默认 2-term 多列 LIKE 的 P95 是否 ≤20ms；另记录更高 term 扇出成本 | 0.5–1 天 | 保持 2-term 上限；若仍超时，限制最近 N 条或将 structured 四表接入 `search_text` + FTS |
| S4 多语言模型 | 中文语义召回是否值得换模型 | 1–2 天 | 保留 `all-MiniLM`，PR-2.5 只留评估报告 |

**关键路径**：S1 → M1 → M2 → S2 → M3 → M4。M4 的部分工作（eval 数据、文档）可与 M3 并行。

### 5.2 M0：基线（PR-0.1）

**目标**：在改代码前，把当前行为和失败用例固定下来。

**改动**：

- 新增 `docs/BASELINE.md`：记录 `cargo test`、`bun test`、`pi-memory-bench` 的当前输出；
- 新增失败测试（先红后绿）：
  - 同一 turn 3 条 fact；
  - retain 重试后 structured 仍在；
  - consolidate 重试只产生 1 条 episodic；
  - episodic 有 embedding；
  - FTS 为空时 vector 仍返回 K 条；
  - 中文“向量”命中；
  - 长 prompt 命中 structured；
  - 结构化时间戳不是 1970；
  - 同一 turn ID 文本变化 → structured 替换而不是幂等短路；
  - 部分 claim 冲突 → 未处理部分仍能前进；
  - 摘要生成期间没有打开事务；
  - 两个实例同时打开旧 DB 时只有一个 FTS 迁移 leader；
  - `clear <target>` 不影响当前 project 的其他表；
  - 迁移只导入目标 scope；
- 加测试辅助：临时 DB、`FakeEmbedder`（按 PR-1.4 的 `EmbeddingProvider` trait 先做最小版本）；
- CI 增加 `cargo test --workspace` + `bun test` 的当前基线（已有，确认不 flaky）。
- 本次复核已用五个临时 probe 验证（probe 文件已删除，不留在 repo）：同一 turn 3 条 fact 当前只落 1 行；`search_text` + FTS `LIMIT` 可命中中文并可限制返回条数；FTS DDL 在事务内可回滚；同一表上 `fts(content)` 与 `fts(search_text)` 可并存；已有 v1 FTS 索引时 `ALTER TABLE ADD COLUMN search_text` + 回填 + 建 v2 索引可正常工作。M0 把前两个固化成正式测试，S1 把后三个纳入迁移测试。

**验收**：失败测试能稳定复现 P0 和关键 P1；基线数字写入文档。

### 5.3 M1：止血（数据正确性 + 中文 + 独立向量）

> **v0.1.17 的范围与顺序**：PR-1.1 → 1.2 → 1.3 → 1.4 → 1.5 → **PR-1.7（先行）→ PR-1.6（依赖 1.7）**。目标是“写入不丢、重试不重复、中文能搜、FTS 为空时向量仍能召回、episodic 有向量、时间戳正确”。PR-1.7 与 PR-1.6 必须同批发布，不允许只发其中一个；完整 RRF 和 structured 词项召回留在 M2，避免止血版引入排序回归。

#### PR-1.1 结构化记忆唯一 ID

**目标**：同一 turn 的 N 条记忆全部落库，重试幂等。

**已复现**：用当前代码对同一 turn 传入 3 条 fact 的 LLM JSON，`count_by_session("facts") == 1`，确认是静默覆盖而不是“只解析了一条”。

**改动**：

- 新增 `crates/core/src/ids.rs`：
  - `stable_id(namespace, kind, parts) -> String`；
  - 规范化：trim、合并空白、小写、去掉首尾标点；
  - hash 用 SHA-256 前 16 hex（新增 `sha2` workspace 依赖）；
- `runtime.rs`：
  - 删除 `format!("{memory_id}:fact:llm")` 这类固定后缀（`143-178`）；
  - 统一 `assign_structured_ids(&mut parsed, memory_id, session, scope)`；
  - 忽略 LLM 返回的 `factId` / `id` / `eventId`；
  - 跳过空 subject/object/preference/instruction；每类最多 32 条，超出截断并记录日志；
- `llm.rs`：保留解析字段，但在注释/接口上明确“ID 由服务端生成”。

**测试**：

- 3 facts / 3 preferences / 3 instructions / 3 timeline → 各 3 行；
- 同一 payload 重试两次 → 行数不变；
- 同一 turn 内两条完全相同 fact → 1 行（内容 hash 去重）；
- LLM 返回伪造 `factId` → 被忽略，不冲突。

**验收**：`cargo test -p pi-memory-core --test llm_integration` 覆盖上述场景。

#### PR-1.2 Retain 事务 + 完成标记

**目标**：消除“working 已写、structured 永远缺失”。

**改动**：

- `TursoStorage`：
  - 新增 `delete_structured_by_source_memory(memory_id)`；
  - 新增 `delete_embedding(kind, id)`（用于文本变化时让旧 working 向量失效）；
  - 复用现有的 `begin_tx/commit_tx/rollback_tx`（`turso.rs:277-291`）；
- `retain_turn_memories`：
  - **LLM/规则提取在事务外完成**；事务只负责 working/structured/session_state/完成标记的 DB 写入。若 extractor 需要读 working 旧内容，先读快照，再在事务内复核 `source_sha`；
  - 先计算 `source_sha = sha256(text)`，再读取 `get_working(id)`；需要基于旧 working 内容提取时，记录读取到的 `snapshot_sha`；
  - **提取期间快照冲突**：事务外提取完成后，开启写事务并复核当前 working 的 `source_sha`/内容版本与提取时快照一致；不一致则 rollback，返回 `snapshot_changed` 和当前 `source_sha`，不写 working、structured、session_state 或 embedding。**Rust 不得用旧 `llm_result` 重试**：由调用方重新读取当前 working/turn 文本并重新调用 LLM；规则路径可重新读取当前文本并重跑规则。最多自动重试 2 次，仍冲突则返回可重试错误并记录诊断，不无限循环。若调用方只有 pending 文本而 working 已有更新，以 working 最新内容为准，不能用旧 pending 文本覆盖；无法判断时标记冲突并交给诊断/人工。首次创建且尚无 working 的 turn 也须在写入前确认不存在同 ID 的并发创建；
  - **已 consolidated 的 working 被更新**：若 `source_sha` 变化且 `consolidated_at` 非空，在同一事务内先让旧摘要失效：找到 `summary_of` 包含该 id 的 episodic，设置 `valid_until=now`；再把这些 episodic 的 `summary_of` 中所有 working id 的 `consolidated_at` 清空并释放 consolidation lease（旧摘要覆盖的整组一起重算，避免只重算一部分造成覆盖缺口）；随后再更新 working/structured。下一次 consolidation 重新生成 episodic；旧 episodic 由 PR-2.4 的 `valid_until` 过滤，不再参与召回。受影响集合超过上限（默认 200 个 working id）时，**仍然在同一事务内清空这些 id 的 `consolidated_at`/lease 并让旧 episodic 失效**，但不在此请求内生成新摘要，改为持久化 `consolidation_dirty=true` 交给后台任务；旧 episodic 仍必须立即失效，不能继续返回；
  - 若 `get_working(id)` 存在且 `metadata.retain == "llm_complete"`：
    - `metadata.source_sha == 新 hash` → 直接返回（真正幂等）；
    - hash 不同 → 进入“更新 turn”事务，不能直接返回；
  - 若 `rule_complete`：
    - hash 相同且本次 `llm_result` 为空 → 直接返回；
    - hash 相同且带 `llm_result` → 允许升级重跑并标 `llm_complete`；
    - hash 不同 → 删除旧 structured，重新提取；无 `llm_result` 时只跑规则并标 `rule_complete`，`diagnose` 标 `llm_re_extract_needed`；
  - 若 `unrecoverable` 且 `working.content` 仍为空 → 直接返回；若本次有非空文本 → 重新走 `working` 状态；
  - 否则进入事务：delete 旧 structured → upsert working（新 content / `updated_at` / `source_sha`）→ insert 四类 structured → save session_state → 写 `retain` 状态/`extractor`/`extracted_at`/`source_sha` → **delete 旧 working embedding（hash 变化时）** → commit；upsert working 时合并 `metadata_json`，保留 `turnIndex` 等已有字段，只覆盖 `retain`/`source_sha`/`extracted_at`；
  - 任何一步失败 → rollback，返回错误；调用方重试会重跑；
  - `metadata_json` 示例：`{ "turnIndex": n, "retain": "llm_complete", "extractor": "llm", "extracted_at": "...", "source_sha": "..." }`；
  - embedding 放在 commit 之后：hash 变化时写新向量（add-before-delete）；模型未就绪则保持缺失，交给回填；hash 未变不重算；
  - 返回结果里带 `updated: bool` 和 `source_sha`，方便调用方/诊断区分“幂等命中”和“内容更新”；
- 旧数据修复：
  - 对 `metadata.retain` 缺失/`working` 的行，只按 `working.content` 跑规则提取；标记 `rule_complete`，**不标记 `llm_complete`**；
  - `working.content` 为空或不可重放 → 标记 `unrecoverable`，只做诊断，不伪造 complete；
  - 旧版 `:fact:llm` 行由 delete-by-source 清掉；
  - 需要恢复旧 LLM 结果时，使用显式 `/memory reextract <turn>`（再次调用 LLM），不在 retain 里自动做。

**测试**：

- 正常 retain 后重试同 turn → working=1、structured 不变；
- 手动插入“只有 working、无 structured”的旧行 → 调用 retain 后规则可提取的项被补齐，状态为 `rule_complete`；
- LLM-only 的旧行（规则提取不到）→ 状态为 `rule_complete` + `extractor=rule`，`diagnose` 显示 `legacy_llm_unverified`，不能断言已补齐；
- `rule_complete` + 新 `llm_result` → 升级为 `llm_complete`，structured 替换；
- 同一 turn ID、文本变化（hash 不同）+ 新 `llm_result` → structured 全量替换，旧 fact 不残留，`source_sha` 更新，返回 `updated=true`；
- 同一 turn ID、文本变化但无 `llm_result` → 状态为 `rule_complete`，旧 LLM-only 项被删除，`diagnose` 标 `llm_re_extract_needed`；
- 同一 turn ID、文本不变 → 返回 `updated=false`，structured/embedding 都不重写；
- 文本变化 → 旧 working embedding 被删除/替换；模型未就绪时保持缺失并可由 backfill 补齐；
- 故障注入（feature `test-failpoints`）：working insert 后、structured insert 前失败 → rollback 后 working 不存在；重试成功；
- 两个并发 retain 对同一 turn 提交不同文本：只有基于当前 source snapshot 的结果能提交；旧提取结果返回 `snapshot_changed`，重提取后收敛到最新文本；
- **旧 `llm_result` 不会在 `snapshot_changed` 后复用**：测试断言冲突返回后没有写入任何 stale structured；
- **已 consolidated 的 working 文本更新** → 旧 episodic `valid_until` 被设置、其 `summary_of` 覆盖的 working 全部重新变成未 consolidated；下一次 consolidation 生成新 episodic，旧 episodic 不参与召回；
- 同一 turn 的旧 structured 被替换，不残留。

**验收**：上述测试全绿；`retain` 的 DB 写入在单事务内完成。

**已覆盖**：`runtime::tests::retain_rejects_stale_llm_snapshot_without_writing_structured_data` 在提取/提交边界通过第二个 storage connection 更新 working，验证 retain 返回 `SNAPSHOT_CHANGED`，保留较新文本、不写 stale fact，也不推进 session checkpoint。运行时按明确可重试错误返回；重新读取并重跑 LLM 由 TS outbox worker 负责。

**已覆盖**：`updating_consolidated_working_invalidates_and_reopens_the_whole_summary_set` 验证更新已汇总 turn 会让旧 episodic 失效，并同时重开其 `summary_of` 整组 working、清空 consolidation lease；新 worker 能 claim 整组并提交带新文本的新摘要。

#### PR-1.3 Consolidation 事务 + 稳定 ID + 摘要 lease

**目标**：Consolidation 的 DB 提交原子、可重试、并发不重复；**LLM/规则摘要生成不持有数据库事务**。

**核心协议（两阶段，先 claim 再摘要）**：

1. **短事务 claim lease**：读取当前 session 的未 consolidate 候选，在事务内对“`consolidated_at IS NULL` 且 lease 为空/已过期”的行写入 `consolidation_lease_owner` / `consolidation_lease_expires_at` / `consolidation_claimed_at`，返回**实际 claim 成功的行集合**及每行的输入版本（`source_sha`，缺失时用 `sha256(content)`）。事务立即提交。claim 成功多少就处理多少，不要求“一次 claim 全部候选”。
2. **事务外生成摘要**：只使用本次 claim 到的 working 内容，调用 LLM 或规则拼接；期间每 `TTL/3` 发送一次 `heartbeat_working_lease(ids, owner)`。摘要结果记录 `summary_of = 精确的 claimed id 列表`、`summary_input_sha = sha256(sorted(id + source_sha))`、`degraded`、`generated_at`。
3. **短事务提交**：重新校验 lease owner、`expires_at > now`、每行仍 `consolidated_at IS NULL` 且 `source_sha` 与 claim 时一致；全部通过才 `insert_episodic` + 批量写 `consolidated_at` + 清 lease，一次 commit。任一行不一致 → rollback，释放自己的 lease，返回 `snapshot_changed`，调用方重新读取候选再跑。
4. **commit 后**写 episodic embedding（best effort）；失败留给 PR-1.4 的回填。

**为什么不是“在事务里 claim + 调 LLM + insert”**：LLM 可能跑数十秒，长事务会持有写锁并阻塞同项目请求，也与 ADR-2“embedding/LLM 不持有写锁”冲突。上面的协议把 claim、摘要、提交拆成两段短事务，崩溃窗口由 lease 过期恢复。

**为什么不是“先生成摘要、再重新 claim 子集”**：摘要输入必须与最终提交集合完全一致；否则 `summary_of` 会覆盖未参与摘要的 working，或摘要包含已被别人处理的内容。lease 在摘要前确定集合，提交前再做版本校验，语义最直接。

**跨进程协议（LLM 在 TS，必须拆 RPC）**：

当前实现是 TS 先 `consolidateSource` 读文本、调 LLM、再 `consolidateSessionMemories(llm_result)`；读取与提交之间没有 claim，摘要集合可能与最终提交集合不同，无法满足 `summary_of` 约束。改成三个 RPC：

1. `claimConsolidation({ sessionId, projectScope, owner, ttlMs, limit })`：Rust 完成 lease claim 并提交，返回 `{ claimToken, expiresAt, working: [{ id, content, sourceSha }], candidatesSeen, partial }`；
2. TS 用 `working[].content` 调 LLM；每 `ttlMs/3` 调 `heartbeatConsolidation({ claimToken })`；心跳失败或超时则停止并调用 `releaseConsolidation`；
3. `commitConsolidation({ claimToken, summary?, degraded })`：Rust 重新校验 lease/版本；`summary` 为空或 `degraded=true` 时由 Rust 用 claim 内容做规则摘要；成功返回 `ConsolidateResult`；
4. 异常路径 `releaseConsolidation({ claimToken })`：只释放该 token 对应的 lease。

- `claimToken` 是服务端生成的不透明 token，绑定 `owner + claimed ids + expires_at`；**不接受客户端传任意 id 列表**来提交，避免旧 client/恶意调用标记未参与摘要的 working。服务端校验 token 时按对应 owner 与 DB 中的 lease 行比对；若 token 只保存在进程内，Rust server 重启后旧 token 直接失效，TS 收到 `CLAIM_EXPIRED` 后重新 claim（旧 lease 需等待过期或由同一 owner 释放）。
- 心跳由 TS 驱动（而不是 Rust 后台线程），这样 TS 进程崩溃/挂起时 lease 会自然过期；Rust 侧只校验。LLM 调用必须是异步/不阻塞事件循环的，否则 heartbeat 定时器无法运行，lease 会过期。
- 兼容：`consolidateSource` 标记 deprecated；旧 `consolidateSessionMemories(llm_result)` 仅在 `PI_MEMORY_LEGACY_CONSOLIDATION=1` 下保留一个版本，且明确“摘要集合可能变化、不保证 `summary_of`”，不能作为新默认路径。v0.1.17 的 TS 与 Rust 同批发布，默认走 claim/commit。

**为什么不是“先生成摘要、再重新 claim 子集”**：摘要输入必须与最终提交集合完全一致；否则 `summary_of` 会覆盖未参与摘要的 working，或摘要包含已被别人处理的内容。lease 在摘要前确定集合，提交前再做版本校验，语义最直接。

**存储改动**：

- `working_memory` 增加（均可空，旧库迁移只加列；`schema_version` 递增，`init()` 先查 `PRAGMA table_info` 再 `ALTER TABLE ... ADD COLUMN`，保证幂等）：
  - `consolidation_lease_owner TEXT`
  - `consolidation_lease_expires_at TEXT`
  - `consolidation_claimed_at TEXT`
- `TursoStorage`：
  - `claim_working_lease(session_id, project_scope, owner, ttl, limit) -> Vec<WorkingClaim>`：单事务条件更新，按 `created_at ASC, id ASC` 取行，保证同一集合重试时摘要输入顺序稳定；返回 `{id, content, source_sha, claimed_at}`（`source_sha` 优先取 `metadata_json.source_sha`，缺失时由运行时计算 `sha256(content)`），不返回“成功条数”；
  - `heartbeat_working_lease(ids, owner, ttl) -> bool`：`WHERE lease_owner=? AND consolidated_at IS NULL`；返回是否全部续期成功；
  - `commit_consolidation(claims, episodic) -> CommitOutcome`：单事务内做 lease/版本校验 + `insert_episodic` + `mark_working_consolidated` + 清 lease；`changes` 不等于 claim 数则 rollback；
  - `release_working_lease(ids, owner)`：只释放自己的 lease；
  - `count_unconsolidated(session_id, project_scope)` 用于返回 `remaining_unconsolidated`；
  - `count_episodic_by_session` 不再参与 ID 生成，仅用于统计/兼容。
- lease 默认 `PI_MEMORY_CONSOLIDATION_LEASE_MS=180000`（大于 LLM 60s 超时），heartbeat 间隔为 TTL/3；heartbeat 失败立即停止摘要，不提交。`claimed_at` / `lease_expires_at` 使用 UTC ISO-8601，保证字符串比较与时间顺序一致。
- 崩溃恢复：进程退出后 lease 到期，其他 worker 可重新 claim 同一批 working；因为上次没有提交 episodic，不会产生半条记录。
- `consolidation_dirty`（由 PR-1.2 在“已 consolidated working 被更新且受影响集合超限”时置位）会强制 PR-3.5 的后台任务至少运行一次；当 `count_unconsolidated == 0` 且没有待处理 dirty 组时才清除。

**ID 与内容覆盖规则**：

- `consolidation_id = stable_id(session_id, "episodic", sorted(claimed_ids))`；**ID 只由 claimed 集合决定，不由摘要文本决定**，同一集合重试得到同一 ID。
- `summary_of` 必须与最终提交的 claimed 集合**完全相等**（顺序无关，集合相等）；沿用现有逗号分隔格式，提交前对 id 排序后 `join(",")`，读取时按集合比较；若未来 id 可能包含逗号，改为 JSON 数组并一次性迁移。代码里用断言/测试保证，不允许“摘要覆盖 A/B/C、提交只标记 A/B”。
- 同一 ID 已存在且 `summary_of` 相同：
  - 若对应 working 已经 `consolidated_at`，说明上一次已提交，本次视为幂等 no-op，不覆盖已有内容；
  - 只有本次 claim 的行仍未 consolidated（异常恢复，或 PR-1.2 的“已 consolidated working 更新后重算”）时才允许更新内容，并记录 warning；正常路径不会走到这里。重算成功时必须清除该 episodic 的 `valid_until`/stale 标记，否则新内容仍会被 PR-2.4 过滤。
- LLM 失败/超时：用同一 claim 集合的规则拼接作为 `degraded=true` 的内容提交，不释放后重试 LLM（避免同一集合反复占用 lease）；调用方可在之后显式重跑摘要。
- 摘要输入按 token 预算截断；需要跨批合并时，每批 ≤N 条，并记录 `source_count`。禁止把已有 episodic 与新 working 反复两两拼接造成 O(n²) 文本增长。

**并发语义**（写进代码注释/文档）：

- 两个 worker 同时 claim **同一完整候选集**：事务串行化，先者拿到 lease，后者 claim 为空 → 1 条 episodic；后者返回 `contended=true` 或 `already_consolidated=true`。
- 两个 worker 读到**部分重叠**的候选集：先者 claim 到 A/B，后者只能 claim 到 C；各自只对**自己的集合**生成摘要并提交，`summary_of` 分别是 `{A,B}` 和 `{C}`，不会出现“摘要覆盖 A/B/C、却只标记 C”的错误。
- 同一 worker 的 lease 在摘要期间过期并被另一个 worker 抢走：原 worker 的提交因 owner/版本校验失败而 rollback；只有新 owner 能提交。
- consolidation 的粒度是“一次 claim 到的集合”，不承诺“一个 session 永远只有一条 episodic”；需要会话级合并时，由后续任务按 `summary_of` 合并，不靠回滚实现。

**返回值**：

- `claimed_working_ids`：本次实际 claim 的集合（也是 `summary_of`）；
- `episodic_count`、`episodic_id`、`episodic_embedded`；
- `degraded`；
- `partial`：本次 claim 集合小于“读取时看到的候选”；
- `remaining_unconsolidated`：commit 后重新计数；
- `snapshot_changed`：提交前校验失败，本次未写入，需要重试；
- `contended` / `already_consolidated`：没有可 claim 的行；
- 调用方在 `partial` / `remaining_unconsolidated>0` / `snapshot_changed` 时重新读取并再跑，后台 drain 最多 3 轮，带 jitter 退避；不能把 partial 当“全部完成”。

**测试**：

- 连续调用两次 → 1 条 episodic；
- **LLM 调用期间无打开事务**：测试用 `FakeSummaryProvider` 在回调里断言 `storage.tx_depth() == 0`（或 failpoint 检查），证明摘要不在事务内；
- 两个线程同时 consolidate 同一完整候选集 → 1 条 episodic，另一方 `contended/already_consolidated`；
- 部分重叠候选 → 两个不相交 claim，各自 `summary_of` 精确覆盖自己的集合，无重复 working id；
- claim 后进程“崩溃”（不提交、不释放）→ lease 过期后另一个 worker 能 claim 并产生 1 条 episodic；
- 摘要期间 lease 被抢走 → 原 worker 提交失败，最终仍有且仅有 1 条 episodic；
- 摘要期间 working 文本被 retain 更新 → 提交返回 `snapshot_changed`，working 保持未 consolidated，重试后成功；
- `summary_of` 与 `claimed_working_ids` 集合严格相等（断言）；
- 同一 claimed 集合重试 → 同一 ID；已有同 ID 且同 `summary_of` 时不覆盖内容；
- 故障注入：commit 的 `insert_episodic` 失败 → rollback，working 的 `consolidated_at` 仍为空、lease 已释放，重试成功；
- LLM 失败 → 规则摘要 `degraded=true`，仍只产生 1 条 episodic；
- **RPC 协议**：`claimConsolidation` 后 TS 延迟 2×TTL（期间正常 heartbeat）能成功 commit；停止 heartbeat 后 lease 过期，commit 被拒绝；
- 旧 `consolidateSessionMemories(llm_result)` 在未开启 `PI_MEMORY_LEGACY_CONSOLIDATION` 时返回 `SNAPSHOT_REQUIRED`，不会用可能过期的摘要提交；
- `remaining_unconsolidated>0` 时调用方再跑一轮能收敛。

**已覆盖**：`expired_consolidation_lease_fences_old_owner_and_recovers_once` 让旧 claim 的 20ms lease 过期，再由新 owner 接管；旧 owner 的 heartbeat 和 commit 均被拒，新 owner 成功提交，最终仅有一条 episodic。

**已覆盖**：`consolidation_dirty_is_project_wide_across_sessions` 验证一个 session 完成时，不会清掉同项目另一个 session 的待汇总标记；dirty 检查与清除由单条条件 UPDATE 完成，避免跨进程 check-then-set 丢标记。

**验收**：`turso_storage` + 新 `consolidation_tx` 测试全绿；代码 review 确认 `consolidate_session_memories` 中没有任何 `begin_tx` 包裹 LLM/规则摘要调用；`summary_of` 与 claim 集合一致的断言存在。


#### PR-1.4 Episodic 向量 + 缺失回填 + 模型完整性

**目标**：让 episodic 真正进入向量索引；旧数据/模型未就绪时的缺口可补齐。

**改动**：

- 常量：`EMBEDDING_MODEL_ID = "all-MiniLM-L6-v2"`、`EMBEDDING_DIM = 384`；
- 引入最小 `EmbeddingProvider` trait（`embed` / `model_id` / `status`），`SharedEmbedder` 实现，测试用 `FakeEmbedder`；这样 PR-1.4 的 episodic embedding 测试不依赖真实模型；
- `SharedEmbedder`：
  - `embed()` 只使用 `Ready`；不再在请求路径同步加载；
  - 新增 `load_now()` / `warmup()` 分离；`prepareModel` 显式等待；
  - **模型完整性（必须和可信预期值比较）**：
    - 新增版本化 manifest（建议 `crates/core/src/model_manifest.rs` 或 `models.lock.json`），记录 `model_id`、上游 revision/commit、每个文件的 `name`/`size`/`sha256`；这是唯一可信来源，不在代码里散落 magic number；
    - 下载 URL 固定到 upstream commit（`/resolve/<revision>/...`），不使用 `main`，避免上游内容漂移；
    - 下载写 `.part` → 按 manifest 校验 size + SHA-256 → 匹配才 rename；不匹配删除 `.part`，换镜像重试，全部失败则 `Failed` 并给出“期望 hash/实际 hash”；
    - 本地 `PI_MEMORY_MODEL_PATH` 默认也按 manifest 校验；自定义模型必须提供 `PI_MEMORY_MODEL_MANIFEST`。`PI_MEMORY_MODEL_VERIFY=0` **只允许在开发/本地构建使用**（见下）；
    - manifest 更新流程：升级模型/revision → 运行脚本重新生成 → 像 lockfile 一样 review；旧 manifest 保留一个版本用于回滚；CI 校验 manifest 格式和文件列表，release 时校验实际下载物；
    - manifest 显式标出 `model_file` / `tokenizer_file`，并列出这两个文件及 ONNX external data 等全部依赖文件；文件名必须安全且不可重复；
  - **`PI_MEMORY_MODEL_VERIFY=0` 的安全边界**：
    - 只在 `debug_assertions` 构建，或显式 `PI_MEMORY_ENV=development` 时生效；release/生产构建下**禁用该变量**（配置报错并拒绝进入自定义模型路径，不能静默降级）；
    - 即使关闭 hash 校验，仍强制：manifest 文件存在且非空、ONNX 可被 Runtime 加载、tokenizer 可解析、实际 hidden size 与 manifest dimension 一致、tokenizer 最大长度和 pooling 与模型配置兼容、文本编码能做一次 smoke encode；
    - 只跳过“文件内容与 manifest 的 size/SHA-256/revision 比对”，不跳过结构/维度/tokenizer 检查；
    - 启动和 `diagnose` 打印 `model_verify=disabled` 的显式 warning，并在 `stats` 中标记 `unsafe_model=true`；
    - 自定义 manifest 必须是用户显式提供的绝对路径；仓库内 manifest 由 review 保证可信，运行时不从网络动态拉取 manifest；
  - **Failed 退避**：`Failed` 记录 `last_error` 和 `next_retry_at`；请求路径只返回降级，不重复同步加载；重试只由后台 warmup / `prepareModel` / `model-status` 触发，指数退避，避免每个 recall 都撞一次损坏模型；
- `crates/core/src/embedding.rs`：
  - `encode_batch` 显式设置 padding（到 batch 最长或模型 max length）、truncation（all-MiniLM 上限 256/512，按模型注册表）、`add_special_tokens=true`；
  - 不再假设 `encode_batch` 自动 padding；用 `encodings.iter().map(len).max()` 或统一 padding 后的长度构造张量；
  - 对超长文本记录一次 warning（带 memory id），避免静默截断；
- `ProjectVectors`：
  - `hydrate(storage, model_id)`：先 reset index/maps；只加载 `embedding.model == model_id`；返回 `stale_model` / `missing` 统计；
  - `save_embedding()` 返回 `EmbeddingOutcome::{Saved, SkippedUnavailable, SkippedEmpty}`；
  - **upsert 顺序改为 add-before-delete**：
    1. DB `save_embedding` 先 upsert（source of truth）；
    2. `index.add(new_vector)` 成功后再更新 `id_to_memory`/`memory_to_id`；
    3. 尝试 `index.remove(old_id)`；失败时只删除旧 mapping，保留 orphan 等待 rebuild/cleanup；
    4. 如果 add 失败：DB 已有新向量，但旧 index/mapping **保留可用**；设置 `needs_rehydrate=true`，下次 recall/start 时从 DB 重建；不返回“成功”；
  - `save_embeddings_batch()` 同样 batch add 后再更新 mapping、再删旧 id；batch add 失败时保留旧索引并标记 rehydrate；
  - 新增 `needs_rehydrate`/`dirty` 标志和 `rehydrate_if_needed()`；索引始终可从 DB 重建，不把内存索引当 source of truth；
- `TursoStorage`：
  - `list_missing_embeddings(kind, model, limit)`：没有 embedding **或** `embedding.model != model` 的行（LEFT JOIN + OR）；
  - `count_missing_embeddings(kind, model)`；
- 新增 `runtime::backfill_embeddings(storage, shared, vectors, budget)`：
  - 每次取 32 条，batch embed，save + index，循环到预算/无缺失；
  - 空文本直接跳过并记录，避免“取到同一批空行 → 永远补不上”的死循环；
  - 返回 `{ scanned, embedded, skipped, remaining }`；
- 调用点：
  - consolidation commit 后写新 episodic；
  - retain commit 后写 working（已有）；
  - `prepareModel` 成功后触发一次**分批、可中断**的回填（不要在一次 RPC 里做全量；PR-3.2 会把它改成后台任务）；
  - `buildRecallInjection` 前可做小预算（≤50ms）的 lazy 回填；
  - `/memory rebuild-embeddings` 全量；
  - `MIGRATE` handler 在 `migrate_from` 成功后重建 `ProjectVectors` 并 `hydrate`，迁移后的数据不重启也能语义检索；source DB 尽量以只读方式打开，避免 `TursoStorage::open` 的 `init()` 改写源库；
- `diagnose`/`stats` 增加 `vector_index_len`、`embeddings_missing`、`embeddings_stale_model`。

**测试**：

- consolidate 后 `memory_embeddings` 有 episodic 行、`index_len` 增加；
- 手动插入无 embedding 的 working/episodic → backfill 后缺失数为 0；
- 插入 `model="old"` 的 embedding → hydrate 不加载，`stale_model=1`；
- 重复 backfill 不增加 index_len（修风险 E）；
- 模拟 `index.add` 失败：旧 vector/mapping 仍可搜；`needs_rehydrate=true`；rehydrate 后新向量生效、旧 orphan 不再出现在结果中；
- 模型未就绪时 retain 仍成功，backfill 在模型就绪后补齐（用 `FakeEmbedder` 或 ignore 的模型测试）；
- 损坏/半截 `model.onnx` → `model_ready()==false`；请求路径不尝试加载；后台重试按退避进行；
- 文件内容被篡改/下载到错误版本 → 与 manifest 的 `sha256` 不匹配，拒绝进入 `Ready`，错误信息包含期望/实际 hash；
- 下载 URL 使用 pinned revision（测试断言不出现 `/main/`）；manifest 的模型/tokenizer 角色引用和全部文件依赖校验通过；
- 自定义 `PI_MEMORY_MODEL_PATH` 未提供 manifest → 默认拒绝；
- `PI_MEMORY_MODEL_VERIFY=0`：debug/dev 构建放行并打印 warning；release 构建禁用并报配置错误；
- 关闭 hash 校验但模型维度错误 / tokenizer 不兼容 / ONNX 加载失败 → 仍拒绝进入 `Ready`；
- manifest 缺失、格式错误、模型/tokenizer 角色未列入文件集或文件名重复 → 构建/CI 失败；
- 不同长度文本 batch embedding 成功（覆盖 padding）；超长文本被截断且记录 warning；
- embedding 结果带 special tokens（用固定 tokenizer 断言首尾 token）。

**验收**：episodic 语义召回测试通过；`diagnose` 能看到缺失/过期向量。

#### PR-1.5 结构化时间戳与来源字段

**目标**：新写入的结构化记忆不再出现 1970；来源/提取器可追踪。

**改动**：

- `crates/core/src/core/extraction.rs`：
  - `ExtractStructuredMemoriesOptions` 增加 `timestamp: String`（取 working 的 `created_at` 或 `now_iso()`）；
  - 规则提取的 fact/preference/instruction/timeline 使用该时间戳，不再对所有项写 `EPOCH_ISO`；
  - `EPOCH_ISO` 只保留给“旧数据/未知时间”的兼容路径；
- `crates/core/src/runtime.rs`：
  - LLM 结果缺时间戳时用当前 turn 的时间，不再补 1970；
  - 写入 `metadata`/`source`：`extractor: "llm" | "rule"`、`source_memory_id`、`extracted_at`；
  - facts 的 `confidence` 缺失时给明确默认值（不要依赖 DB 默认值导致排序不确定）；
- 测试：规则/LLM 两条路径的时间戳都等于 turn 时间；旧行仍可读；`ORDER BY timestamp` 不再把新记忆排到 1970 后面。

**验收**：`extraction` 单测 + `retain` 集成测试；grep 不到新的 `EPOCH_ISO` 写入点（兼容分支除外）。

#### PR-1.7 `search_text` + CJK 查询 + FTS Top-K（原 PR-2.1 最小版，先行）

> **执行顺序：PR-1.7 → PR-1.6。** PR-1.6 的向量独立 Top-K 需要在新 FTS 查询形状下验收；在 PR-1.7 合入前，PR-1.6 只能做单元级实现，不能作为可发布增量。二者必须同批进入 v0.1.17。

**目标**：中文子串/长 query 可检索；FTS 真正走 `LIMIT`；不引入 structured 词项召回和 RRF。

**改动**：

- 按 5.4 PR-2.1 的设计实现 `text.rs`、`search_text` 列、v2 FTS 索引、分批回填、`fts_state` 切换、查询改写和 `LIMIT`（schema/回填/查询改写的实际落地在本 PR，PR-2.1 只做收尾）；
- **本 PR 只做 working/episodic 的 FTS**；structured 仍走旧 LIKE（PR-2.3 再改）；
- 查询 SQL 不带 `project_scope` 谓词，依赖 DB 按项目隔离；若检测到 legacy 单 DB 多 scope，走慢路径并在 `diagnose` 警告；英文 query 只对有区分度的词项生成 FTS OR 查询，忽略 `how/the/and` 等函数词，全部为空时跳过 FTS；中文 bigram 保持不变；
- 保留 v1 `fts(content)` 索引一个版本，`PI_MEMORY_FTS_MODE=content` 可回退；
- 不在本 PR 调 BM25 权重或最终排序。

**测试**：

- “项目使用 turbovec 向量索引”查询“向量”命中；
- 长中文 query 按 bigram 命中；
- 10k 行、常见词、`limit=50` 只返回 50 条；
- `EXPLAIN QUERY PLAN` 不再出现 `USE SORTER FOR ORDER BY`；
- v1/v2 切换失败时 v1 仍可查；
- 英文 query 不回归。

**验收**：`tests/turso_storage.rs` 更新为新语义；新增 `cjk` 与 `topk` 测试；10k 性能写入 PR；合入后才能开始 PR-1.6 的集成验收。

#### PR-1.6 独立向量 Top-K（原 PR-2.2a，依赖 PR-1.7）

> **依赖：PR-1.7 已合入。** 本 PR 不再以“FTS 查询形状待修”为前提；所有 FTS 为空的测试必须在 PR-1.7 的新查询路径上运行。**发布门禁：PR-1.7 + PR-1.6 必须同批进入 v0.1.17，不允许只发其中一个。**

**目标**：FTS 为空或候选很少时，向量仍返回 K 条；为 M2 的完整 RRF 打底。

**改动**：

- `ProjectVectors::search_global(query, k) -> Vec<(kind, id, content, score)>`：不传 allowlist，直接全局 Top-K；
- `build_embedding_candidates` 的 allowlist 路径保留为 `allowlist` 调试/兼容模式，不再用于 recall；
- `runtime::build_recall_injection`：
  - FTS 候选来自 PR-1.7 的新查询路径（`LIMIT` 下推、`search_text`）；
  - 向量改为全局 Top-K（`PI_MEMORY_VECTOR_TOP_K`，默认 50）；
  - 暂不引入 RRF：沿用现有 `recall_memories` 合并，但保证向量候选能进入最终 6 条；
  - FTS 为空时，向量返回 `min(K, index_len)`，不再 `max(1)`；
- 新增 `PI_MEMORY_RECALL_MODE=legacy` 开关，回退到 allowlist 行为一个版本；
- 性能：10k 行、`VECTOR_TOP_K=50`，召回 P95 不超过基线 2 倍。

**测试**：

- 在新 `search_text` 路径上，FTS 为空 → vector 返回 K 条（不是 1 条）；
- 在新 `search_text` 路径上，FTS 有 1 条无关候选、vector 命中真正相关 → 最终 Top-6 包含 vector 命中；
- 关闭 embedding（`FakeEmbedder` 返回 None）→ FTS/structured 正常；
- `legacy` 开关行为与旧版一致；
- 用 `PI_MEMORY_FTS_MODE=content` 回退时，向量仍全局 Top-K（不重新引入 allowlist）。

**验收**：PR-1.7 已合入；`hybrid_recall.rs` 在新 FTS 路径上通过；perf 记录写入 PR；v0.1.17 发布清单同时包含 PR-1.7 和 PR-1.6。

**已覆盖**：`hybrid_recall.rs::vector_hit_outside_fts_is_included_in_hybrid_recall` 验证一条无关 FTS 候选不会限制相关 vector 候选进入最终注入；`vector_top_k_is_nonempty_when_fts_returns_no_candidates` 验证 FTS 为空时仍返回索引中全部 3 条向量候选。

### 5.4 M2：召回质量与结构化（P0/P1）

> PR-2.1 和 PR-2.2 的“最小独立 Top-K”部分已前移到 M1（PR-1.6 / PR-1.7）。本节只描述 M2 的剩余工作：完整 RRF、structured 词项召回、元数据/去重、clear/迁移安全、多语言评估。

#### PR-2.1 `search_text` + CJK 查询分析 + FTS Top-K（M2 收尾）

> **注**：schema、`text.rs`、回填、v2 索引、查询改写已在 PR-1.7 落地。本 PR 在 M2 收尾：补 legacy 单 DB 多 scope 慢路径、迁移/回滚演练，以及 PR-1.7 遗留的查询形状问题。当前 10k 合成数据性能门槛通过；真实项目数据复核仍需要项目语料。`ngram` 单行 spike 已验证基本 CJK/英文查询均能命中，但未跑 10k 性能与召回比较，因此不发布该模式，继续默认使用 `search_text`。

**目标**：中文子串/长 query 可检索；FTS 真正走 `LIMIT` 和 BM25。

**改动**（设计参考；schema/回填/查询改写的实际落地在 PR-1.7，本 PR 只做收尾与复核）：

- 新增 `crates/core/src/text.rs`：
  - `normalize`：全角转半角、小写、去控制字符；
  - `terms(input) -> Vec<String>`：英文/数字词 + CJK 连续段的 2-gram（单字段保留单字），去重，最多 32；
  - `build_search_text(input) -> String`：英文词 + CJK bigram，空格连接；
  - `to_fts_query(terms) -> String`：`"t1" OR "t2" ...`；
- schema：
  - `working_memory.search_text`、`episodic_memory.search_text`；
  - 新建 v2 FTS 索引 `fts_working_idx_v2` / `fts_episodes_idx_v2`（`USING fts (search_text)`），旧 v1 保留一个版本；通过 `fts_state` 切换（见 8.1）；
  - `app_meta` 记录 `schema_version` / `fts_version`；版本变化时按 8.1 的状态机迁移，不直接 `drop_fts()`；只有 v0.3 正式移除 v1 时才允许 drop 旧索引；
  - 旧数据 backfill `search_text`（分批，避免一次性大事务）；
- `TursoStorage`：
  - `insert_working` / `insert_episodic` 计算并写入 `search_text`；
  - `rebuild_fts` / `/memory rebuild` 按 `fts_state` 重建对应索引：`content` 重建 v1，`search_text` 重建 v2；迁移期不要无条件 `DROP` 掉 v1；
  - `search_working_fts(query, limit)` / `search_episodic_fts(query, limit)`：
    ```sql
    SELECT ..., fts_score(search_text, ?1) AS score
    FROM working_memory
    WHERE fts_match(search_text, ?1)
    ORDER BY score DESC
    LIMIT ?2
    ```
  - 不再在 SQL 里带 `project_scope`；DB 已按项目隔离。若检测到单 DB 多 scope（旧迁移数据），走 `legacy_scoped` 慢路径并在 `diagnose` 警告；
  - 返回真实 BM25 分数，不再在运行时算百分位；
  - spike 时确认“多列 SELECT + LIMIT”仍能匹配 Turso 的 FTS 模式；如果额外列导致不匹配，改成两步：先 `SELECT rowid, fts_score(...) ... LIMIT` 拿 id，再按 id 取行。
- 可选 spike：单行探针已验证 `WITH (tokenizer='ngram')` 的基本 CJK/英文命中；要启用仍需真实规模的性能、排序和召回对比。当前 `PI_MEMORY_FTS_MODE=ngram` 明确报错；未完成该门槛前不得把它写成可用配置。

**测试**：

- “项目使用 turbovec 向量索引” 查询 “向量” 命中；
- 长中文 query 按 bigram 命中；
- 10k 行、常见词、`limit=50` 只返回 50 条；
- `EXPLAIN QUERY PLAN` 不再出现 `USE SORTER FOR ORDER BY`（确认 limit 下推）；
- BM25 分数单调，且不是 `1 - index/(total+1)`；
- 英文 query 不回归；
- 英文长 prompt 的函数词不产生大量无关 FTS 候选；仅含函数词时安全跳过 FTS，中文 bigram 查询不受影响；
- 旧 DB 迁移后 `search_text` 回填、FTS 可查。

**验收**：`tests/turso_storage.rs::turso_tantivy_fts_match_search` 更新为新语义；新增 `cjk` 与 `topk` 测试。

#### PR-2.2b 双路召回 + RRF（M2；2.2a 已在 PR-1.6）

> **拆分**：PR-1.6 已完成“独立向量 Top-K”（2.2a）。本 PR 是 2.2b：完整 RRF、权重、阈值、确定性 tie-break、性能与回滚开关。

**目标**：向量召回不再受 FTS 候选集限制；FTS 不再压过向量；无关候选不注入。

**改动**：

- 复用 PR-1.4 的 `EmbeddingProvider` trait（`SharedEmbedder` 实现；测试用 `FakeEmbedder`）：
  ```rust
  pub trait EmbeddingProvider: Send + Sync {
      fn embed(&self, texts: &[String]) -> Option<Vec<Vec<f32>>>;
      fn model_id(&self) -> &str;
      fn status(&self) -> String;
  }
  ```
- `ProjectVectors`：
  - `search_global(query, k) -> Vec<(kind, id, content, score)>`；
  - 保留 allowlist 方法仅用于调试/兼容，不再用于 recall；
- `runtime::build_recall_injection`：
  - `FTS_TOP_K=50`（working + episodic 各自，或合并后取 50）；
  - `VECTOR_TOP_K=50`；
  - `STRUCTURED_TOP_K=30`；
  - 三路独立取候选，交给 `recall_memories`；
  - 最终 `limit=6`（token budget 在 M4）；
- `core/recall.rs`：
  - `RecallCandidateInput` 增加 `rank`/`source` 或由调用方按顺序传入；
  - `RecallMemoriesOptions` 增加 `rrf_k`、`weights`、`min_score` / `relative_cutoff`；
  - 合并改用 `HashMap<(MemoryKind, String), Accum>` 按 ID 聚合，替代当前 `merged.iter().position(...)` 的线性查找（O(n²)）；
  - `score = Σ w_s / (rrf_k + rank_s)`；同分按 `memory_id`；
  - **相关性阈值**：`rrf_score < min_score` 或低于最高分 `relative_cutoff` 的候选丢弃；全都不达标时返回空，而不是硬凑 6 条；
  - `RecallScoreBreakdown` 改为逐源贡献 + rank，保留诊断；
- 配置（环境变量，默认值即推荐值）：
  - `PI_MEMORY_FTS_TOP_K=50`
  - `PI_MEMORY_VECTOR_TOP_K=50`
  - `PI_MEMORY_STRUCTURED_TOP_K=30`
  - `PI_MEMORY_RRF_K=60`
  - `PI_MEMORY_RECALL_MIN_SCORE=0.012`（约等于 rank 1 的 `1/61` 下限；按 eval 调整）
  - `PI_MEMORY_RECALL_RELATIVE_CUTOFF=0.2`（40 条跨语言诊断集中，0.5 会丢弃 vector rank 1–5 的独立候选；0.2 降低该损失）

**测试**：

- FTS 为空 → vector 返回 K 条，不再是 1 条；
- FTS 有 1 条无关候选、vector 命中真正相关 → 最终 Top-6 包含 vector 命中；
- FTS 有候选但都没有向量 → vector 仍全局搜索；
- RRF 排序确定、tie-break 稳定；
- 无关候选全部低于阈值 → 注入为空（或只保留最高且达标的 1 条）；
- 10k 候选规模下合并不出现二次方退化（基准：3×50 候选 < 5ms）；
- 关闭 embedding（FakeEmbedder 返回 None）→ FTS/structured 正常；
- 性能：10k 行、`FTS_TOP_K=50`，召回 P95 不超过基线 2 倍；若超过，先确认 FTS `LIMIT` 是否真正下推、vector 是否仍在做全量重排。

**验收**：新增 `hybrid_recall.rs` 测试；原 `recall.rs` 单测更新为 RRF 语义。

#### PR-2.3 结构化记忆按词项召回

**目标**：正常长 prompt 能命中 facts/preferences/instructions/timeline，而不是 `LIKE %整句%`。

**改动**：

- `TursoStorage`：
  - 新增 `search_facts_terms(terms, scope, limit)`；
  - 同理 preferences/instructions/timeline；
  - SQL 动态构造：每个 term 对 `subject/predicate/object` 做 `LIKE '%term%'`，OR 连接；term 上限 16，避免参数爆炸；
  - 过滤：instructions 加 `active = 1`；structured 四表当前没有 `valid_until` / `superseded_by`，本 PR 不加列，过期机制只覆盖 working/episodic（见 ADR-7）；
  - 排序：facts 按 `confidence DESC, timestamp DESC`；其余按 `created_at DESC`；
  - `LIMIT ?`；
- `runtime::query_structured_candidates`：
  - 用 `text::structured_recall_terms(prompt)`，空则跳过；优先保留较长/去停用词的 term，并从长 prompt 尾部采样后默认最多选择 2 个（`PI_MEMORY_STRUCTURED_TERM_LIMIT` 可配置，最高 3），避免把“怎样/避免/多个”这类噪声和高 LIKE 扇出带入查询；
  - structured score：facts `0.4 + 0.2*confidence`，其余 0.5；作为 RRF 之外的 tie-break/诊断；
- 保留旧 `search_*_like` 作为 legacy 测试/迁移，不再用于 recall。

**性能门槛（不再是“记录一下”）**：

- facts/preferences/instructions/timeline 四个表各 10k 行的合成数据上，structured 查询 P95 ≤ 20ms，`buildRecallInjection` 端到端 P95 ≤ 100ms；
- 如果超过：默认 term 降到 2 个、只查当前 project 的最近 N 条，或把 structured 四表也接入 `search_text` + FTS（与 working/episodic 同一方案）；
- 在 PR-2.3 合并前跑一次 10k bench；不达标不允许把 structured terms 设为默认路径，先保留 legacy `LIKE` + 开关。

**测试**：

- fact `"回复使用中文"`，prompt `"怎样避免多个智能体同时修改代码冲突，回复使用中文"` → 命中；
- prompt 为整句但 fact 是子串 → 命中；
- `active=0` instruction 不命中；
- 过期 working/episodic 不命中（structured 的过期不在本 PR 范围）；
- structured 四表各 10k 行、最终默认的 2 terms 路径 P95 ≤ 20ms（写入 benchmark 输出，作为合并门槛）；更高 term 数仅用于衡量未限流时的成本，不允许作为默认路径。

**验收**：e2e 改用真实长 prompt，不再用 `"中文"` 这种特例；10k bench 的 structured P95 ≤ 20ms，否则先走 FTS 回退方案，不默认开启。

**已覆盖**：`llm_integration::recall_with_llm_extracted_structured` 使用长中文 prompt 验证事实子句召回，并断言 inactive instruction 不进入结果。

#### PR-2.4 元数据生效 + consolidated 去重 + 模型版本过滤

**目标**：现有字段不再是死字段；旧模型向量不污染召回；consolidated working 不重复注入。

**改动**：

- **consolidated working 排除**：在 `ProjectRuntime` 维护 per-project eligibility cache（`HashSet<(MemoryKind, id)>` 的 consolidated working），hydrate 时加载，consolidation/clear 时更新；召回拿到 Top-K 后再过滤，**不要**在 FTS SQL 里加 `consolidated_at IS NULL`（会破坏 PR-1.7 的 `LIMIT` 下推）；
- **去重**：episodic 的 `summary_of` 包含的 working id 从最终候选移除（保留 episodic）；内容 hash 完全相同的候选只保留分数最高的一条；`diagnose` 输出被去重的数量；
- working/episodic 的 FTS 结果后过滤 `valid_until` / `superseded_by`；空字符串按“未设置”处理，避免历史数据误排除；
- structured SQL 只做 `active` / `confidence`（PR-2.3）；如果产品要求 fact 级过期，再单独加列，不在本 PR 顺手加；
- vector 候选：在 `ProjectVectors` 中维护 working/episodic 的 `valid`/`expires_at`（hydrate 时加载；consolidation/clear 时更新），或对 Top-K 做一次 `get_working/get_episodic` 校验；优先前者；
- `instructions.active` 过滤；
- `facts.confidence` 参与排序（PR-2.3 已做）；
- `hydrate` 只加载当前 model；`diagnose` 显示 stale 数量；
- 新增 `purge_expired`：在单事务内用集合式 SQL 删除过期 working/episodic 与对应 embeddings，返回计数而不把全部 ID 载入内存；启动/`/memory purge` 触发，随后重建向量索引并保留 FTS dirty 状态。

**测试**：

- 过期 working/episodic 不出现在召回；
- `superseded_by` 非空的记忆不出现；
- consolidated working 不出现在召回；episodic 的 `summary_of` 对应的 working 被去重；
- consolidation 后同一进程内 cache 立即生效，不需要重启；
- 旧 model 向量不被加载；
- purge 后 DB/索引一致。

**验收**：元数据查询测试全绿；`diagnose` 可解释“为什么某条没召回”。

#### PR-2.5 多语言 embedding 评估（条件性 P0/P1）

**目标**：如果中文是一等场景，验证 `all-MiniLM-L6-v2` 是否足够；不够则切换。

**改动**：

- 建立 30–50 条中英文标注 query（包含“怎样避免多个智能体同时修改代码冲突” → “Git Worktree 隔离并行开发任务”这类语义匹配）；
- 对比：
  - `all-MiniLM-L6-v2`（现状，384）
  - `paraphrase-multilingual-MiniLM-L12-v2`（384）
  - `bge-small-zh-v1.5`（512）
  - `multilingual-e5-small`（384）
- 指标：Recall@5、MRR、P50/P95、模型大小、冷启动；
- 提供本地候选模型对比入口：每个候选使用独立临时 DB、相同 fixture 和 query 集；模型配置由 pinned manifest 提供维度、输入/池化和 query/document 前缀；
- 如果切换：
  - `Embedder` 支持 2/3 输入、动态 hidden size、pooling（mean/cls）；
  - manifest 驱动的模型配置（id、dim、files、pooling、可选 query/document prefix）；
  - `memory_embeddings.model` 写入新 id；旧向量 hydrate 时忽略；
  - 每个 memory/model id 单独保留 embedding，模型回滚可复用旧向量；
  - schema version 递增，旧的 `(memory_kind, memory_id)` 主键幂等迁移为 `(memory_kind, memory_id, model)`；
  - `rebuild-embeddings` 全量重建；
  - 迁移文档/回滚开关 `PI_MEMORY_EMBEDDING_MODEL`。

**决策门槛**：在中文标注集上，新模型 Recall@5 相对现状提升 ≥10 个百分点，且 P95 不退化 >20%，才默认切换；否则保留现状，仅提供可选模型。用户已选择继续使用当前 `all-MiniLM-L6-v2`，不因缺少候选模型语料/标注而暂停其余优化；因此当前不切换模型，候选比较不作为本次完成条件。

**估算**：评估 1 天；切换实现 2–3 天。

**当前证据（2026-11-23）**：`eval/semantic-recall.jsonl` 有 40 条跨语言释义，覆盖 20 个目标记忆；英语 query 指向中文记忆，中文 query 指向英语记忆，nightly/手动 workflow 会同时运行。`--explain-misses` 证实 common English function words 制造了无关 FTS 候选，旧的 0.5 relative cutoff 还会删除高排名的单路 vector 结果。现已在 v2 FTS 查询中忽略英文函数词，并把默认 relative cutoff 调到 0.2。当前模型的 regular 40-case bilingual fixture hybrid Recall@5/10 为 0.775/0.850，MRR 0.6525，nDCG@10 0.6941；semantic fixture hybrid Recall@5/10 为 0.525/0.650，vector-only Recall@5/10 为 0.500/0.650。中文 semantic 子集 hybrid/vector Recall@5 为 0.10/0.00，跨语言中文语义召回仍弱。**用户已明确选择保留当前模型**；这组指标作为当前模型的合成基线，不声称代表真实项目质量，也不触发模型切换。详见 `eval/README.md`。

#### PR-2.6 `/memory clear` 语义 + 迁移 scope 安全（从 M4 前移）

**目标**：破坏性操作只影响目标项目；迁移不把其他项目的记忆混进来；**明确区分 DB 事务和事务外索引重建，不再承诺跨步骤整体回滚**。

**clear 改动**：

- `runtime::memory_command("clear")`：
  - `target` 为空 → 当前 `project_scope`；
  - `target` 非空 → 通过 server `StoragePool` 打开目标 project 的 DB 再清，不再用当前 DB 的全局表；
  - **DB 删除在单事务内完成**：先按 scope 收集待删除的 working/episodic id（用于清向量），再删除 `working_memory` / `episodic_memory` / `facts` / `preferences` / `instructions` / `timeline` / `session_state` / `memory_embeddings`；embeddings 通过 `(memory_kind, memory_id)` 关联删除；并在同一事务中持久化 `fts_dirty=true`、`needs_rehydrate=true`，防止 DB 删除提交后、内存索引失效前进程异常造成旧索引被继续信任；
  - `clear all` 表示“所有 project scope”，不是“所有表无过滤”。除枚举项目 DB 外，还要枚举每个 DB 内的 logical `project_scope`，覆盖旧版单 DB 多 scope 迁移数据；实现为逐 scope 事务、每个 DB 只重建一次索引；跨 scope 不承诺单事务，返回每个 scope 的状态；dirty 标记按 scope 持久化（例如 `fts_dirty:<scope>` / `needs_rehydrate:<scope>`），或使用等价的 per-scope 状态表；只有在所有 scope 重建成功后才清除全局汇总标记，避免一个 scope 重建成功就清掉另一个 scope 的待恢复状态；
  - 全项目清理必须显式 `all` + 二次确认（`confirm` 文案写明不可恢复）；
- **事务外重建**：commit 后重建目标 project 的 FTS 和向量索引（`hydrate`/`rebuild-embeddings`）。若重建失败：
  - **不能返回“clear 失败、已回滚”**，因为 DB 删除已经提交；
  - 返回 `{ db_committed: true, deleted: {...}, index_state: "partial", fts_dirty: true, needs_rehydrate: true, error }`；
  - 清空内存中的 `ProjectVectors`/eligibility cache，避免继续用旧向量召回；查询降级为 FTS/DB-only；
  - 后续 `/memory rebuild` 或下一次启动自动重试重建，直到 `index_state=ok`；成功后才清除持久化的 dirty 标记；启动时若发现 dirty 标记，先禁止使用旧向量索引并执行 rehydrate；
- `clear` 结果同时包含 DB 状态和索引状态；`diagnose` 显示 `fts_dirty` / `needs_rehydrate` / 上次重建错误。

**migrate 改动**：

- `TursoStorage::migrate_from`：
  - 增加 `target_scope` 参数；只导入匹配 scope 的行（或按 scope 拆分目标 DB），不再 `list_all_*` 全量导入；
  - 源库以只读方式打开，避免 `TursoStorage::open` 的 `init()` 改写源库；
  - **导入在目标 DB 的单事务内完成**：批量插入 + 迁移前后计数校验；计数不一致 → rollback，目标 DB 保持迁移前状态；
  - **同一导入事务内持久化恢复标记**：`index_state=partial`、`fts_dirty=true`、`needs_rehydrate=true`；只有 FTS/向量重建成功后才清除。这样导入 commit 后、重建前崩溃，重启仍会先降级/重建，不会误信空索引或旧向量；
  - 若目标 DB 是本次新建且导入事务失败，可以直接删除/重建目标文件；若是已有 DB，则只依赖事务回滚；
- **事务外重建**：导入 commit 后重建 FTS + `ProjectVectors::hydrate`。若重建失败：
  - 导入数据保留，返回 `{ db_committed: true, import_status: "imported", index_state: "partial", fts_dirty: true, needs_rehydrate: true, skipped_scopes, error }`；
  - 索引重建可重入；重试不会重复导入（按主键 upsert/存在性检查）；
  - 不把“索引重建失败”报告成“迁移失败、目标 DB 已回滚”；
- 兼容：`migrate` 不带 scope 时默认当前 project scope，并在输出里提示被跳过的 scope 数量。

**测试**：

- 项目 A 执行 `clear`，项目 B 的 working/episodic/structured/embeddings 仍在；
- `clear all` 必须带确认；无确认时拒绝；
- **clear 的 FTS/向量重建注入失败** → DB 行已删除、`db_committed=true`、`index_state=partial`、`needs_rehydrate=true`；再次 rebuild 后恢复 `ok`，且不会“复活”已删除记忆；
- 迁移只导入目标 scope；源库 mtime/内容不变；
- **迁移导入事务注入失败** → 目标 DB 保持迁移前状态（新建 DB 可删除重建）；源库可继续使用；
- **迁移导入成功但 FTS/hydrate 注入失败** → 数据可读（DB 已提交）、返回 `index_state=partial`；重建后召回正常，不重复导入；
- **迁移导入事务提交后、索引重建前崩溃** → 重启读到持久化的 `index_state=partial`/`fts_dirty`，先降级，不把空索引当健康；重建后恢复；
- 迁移后召回不混入其他项目。

**验收**：双项目 clear/迁移测试；索引重建失败用例；`diagnose` 输出 scope 分布与 `index_state`；release notes 说明旧版 `clear <target>` 可能已造成的数据损失。**文档和返回值不得再使用“迁移整体回滚”这种超出事务边界的措辞。**


### 5.5 M3：可靠性与成本（P1）

#### PR-3.1 Server 并发 + socket 生命周期

**目标**：慢请求不阻塞所有项目；冷启动只有一个 server。

**改动**：

- `StoragePool`：
  - `HashMap<String, Arc<Mutex<ProjectRuntime>>>`；
  - `get_or_open` 只锁 map，返回 `Arc`；
  - 请求处理时只锁目标 project；
  - `prepareModel` 不持有 project 锁；
- **锁中毒与连接计数**：
  - 请求路径不要 `lock().expect("mutex poisoned")`；改用 `parking_lot::Mutex`（无 poison）或显式恢复 `PoisonError::into_inner()`；
  - 无法恢复时返回结构化错误并标记 server unhealthy，由 client 重启，而不是让线程继续 panic；
  - `conn_count` 用 RAII guard 或 `catch_unwind` 包裹 `handle_socket_conn`，保证 panic 时也 `fetch_sub`；否则连接数泄漏会让 server 永不退出；
  - 每个连接线程的 panic 记录日志（不含记忆内容）；
- **内存上限**：
  - `ProjectVectors::content_map` 改为有界（按项目 LRU/容量上限）或只存 `(kind,id)`、召回时按需查 DB；项目被 LRU 驱逐时整体 drop；
  - `id_to_memory` / `memory_to_id` 与 `content_map` 同步清理，避免孤儿 mapping；
- **权限与隐私**：
  - DB 目录 `0700`、DB 文件 `0600`；socket 目录 `0700`、socket `0600`；
  - debug log 不输出记忆正文、prompt、LLM payload；只记录 id/长度/错误类型；
  - `diagnose` 不打印记忆内容；
- socket：
  - 新增 `fs2`（或等价 `flock`）依赖；
  - `run_socket_server`：先 `connect` 探测；成功则退出（已有 server）；
  - 获取 `socket_path.lock` 排他锁；成功后再 `remove_file` stale socket + bind；
  - 设置目录 `0700`、socket `0600`；
  - 退出时先删 socket，再释放 lock；
- TS：
  - `socketModeEnabled()` 在 `win32` 返回 false；
  - 更新 README/leaf package 说明；
- 可选：项目 runtime LRU（`PI_MEMORY_MAX_OPEN_PROJECTS`），避免长期运行内存膨胀。

**测试**：

- 两个 client、不同 project，同时发慢请求（用 `migrate` 大文件或 test-only delay）→ 互不阻塞；
- 两个进程同时冷启动 → 只有一个监听，另一个退出/复用；
- socket 权限断言；
- stale socket（进程已死）能被恢复；
- 连接处理线程 panic 后 `conn_count` 归零，server 能按 idle/退出逻辑结束；
- 锁中毒时请求返回结构化错误，服务可被 client 重启；
- `content_map` 达到上限后按 LRU 裁剪，召回仍能从 DB 补内容；
- DB 文件/目录权限为 0600/0700；
- Windows 单测：`socketModeEnabled()` 返回 false（CI 可跳过）。

**验收**：并发测试 + 冷启动测试稳定。

**已覆盖**：`pi_memory_server::tests::busy_project_runtime_does_not_block_another_project` 在独立线程持有项目 A 的 runtime 锁时，验证项目 B 能获取自己的 runtime 锁并完成一次 DB 访问。`crates/core/tests/socket_server.rs` 增加了双进程同时冷启动、单监听者、目录/socket/lock 权限和 shutdown 清理验证；当前受限沙箱禁止 AF_UNIX `bind`（`Operation not permitted`）时会显式跳过，其余失败仍会报错。

#### PR-3.2 RPC 超时分级 + 不销毁共享连接

**目标**：单请求超时不影响其他请求/会话；同时明确“超时 ≠ 取消”。

**语义（必须写进代码注释/文档）**：

- 客户端超时只是“放弃等待并删除 pending”，**服务端可能仍继续执行完**；
- 当前没有取消协议；不要因为超时就假设写操作没发生；
- 同一连接上的请求是串行处理的：一个慢请求超时后，服务端可能仍在跑，后续请求会排队；超时不释放服务端容量；
- 因此所有写请求必须幂等：`retain` 用确定性 ID + 事务；`consolidate` 用 claim lease + stable id + 提交前版本校验；`enqueueTurn` 用 `(session_id,turn_index)` 主键；`clear` 用确认 + 幂等删除；`migrate` 不自动重试，改为 job/status；
- 长操作（`prepareModel`/`migrate`/`rebuild`/`rebuild-embeddings`/`clear`/`purge`）改为“启动 job + 立即返回 + 轮询状态”，不靠长超时占着连接；`rebuild` job 依次重建 FTS 与内存向量索引，并在失败时保留 `partial` 恢复状态；`clear all confirm` 的每个 scope 状态保存在 job 详情中。

**改动**：

- `client.ts`：
  - 方法超时表：`hello/recall/retain/enqueue` 5–10s；`claimConsolidation` 10s；`heartbeatConsolidation` 5s；`commitConsolidation` 30s；job 启动类 5s；状态轮询 5s；
  - `invoke` 超时只 reject 当前请求、从 pending 删除，**不 destroy**；
  - 迟到的 response 直接忽略（request id 不复用）；
  - 只有 socket `close`/进程 `exit` 才走重连；
  - 连续 N 次超时/连接无响应才标记 transport unhealthy，避免单次慢请求触发重连风暴；
  - `shutdown` 在 socket 模式只 `conn.destroy()`，不发全局 shutdown；
- `server`：
  - 新增 `startJob`/`jobStatus`（或按方法返回 `{jobId}`），用于 `prepareModel`/`migrate`/`rebuild`/`rebuild-embeddings`/`clear`/`purge`；
  - `migrate` 只锁目标 project；job 内部仍要保证幂等/可重试；
  - 可选：为 `retain/consolidate` 加 `idempotencyKey`；当前确定性 ID/claim 已可重试，先不加协议字段。

**测试**：

- 一个 slow call 超时后，同一连接上的下一个 call 成功（或在服务端忙完后成功）；
- 两个 pending 请求，一个超时，另一个不被 reject；
- 超时的 `retain` 最终在服务端完成；客户端用同一 turnIndex 重试不会产生重复行；
- socket 模式一个 client shutdown，另一个 client 仍可用（server 不退出）；
- stdio 模式超时不 kill 进程（除非显式 shutdown）。

**验收**：新增 `timeout.test.ts`；现有 smoke/lifecycle 不回归；文档明确“at-least-once + 幂等写”。

#### PR-3.3 `turn_end` outbox + user prompt 捕获

**目标**：`turn_end` 不阻塞；LLM 在后台；用户 prompt 不丢。

**改动**：

- schema：`pending_turns(session_id, turn_index, project_scope, text, status, lease_owner, lease_expires_at, attempts, next_attempt_at, last_error, created_at, updated_at, PRIMARY KEY(session_id, turn_index))`；
- RPC：
  - `enqueueTurn`（幂等 insert，`ON CONFLICT(session_id,turn_index) DO NOTHING`）
  - `claimPendingTurn(sessionId, owner, leaseMs)`（事务内条件更新；返回 `None` 表示没有可 claim 的行）
  - `heartbeatPendingTurn(sessionId, turnIndex, owner, leaseMs)`（可选，LLM 超过 lease 时）
  - `completePendingTurn(sessionId, turnIndex, owner)`
  - `failPendingTurn(sessionId, turnIndex, owner, error)`
  - `listPendingTurns(sessionId, limit)`（诊断/管理，不参与 claim）
- `ts/index.ts`：
  - **session 标识**：`resolveSessionId` 不再回退到全局 `"default-session"`；优先 `sessionManager.getSessionId()`，其次 `PI_SESSION_ID`，都没有时用进程内随机 id 并 warning。subagent 必须有独立 session id，否则 `(session_id, turn_index)` 会冲突；
  - `before_agent_start`：按 session 缓存 `lastPrompt`，`turn_end` 使用后清空，避免 custom message 复用旧 prompt；
  - `turn_end`：
    - 跳过 `event.outcome === "aborted"`；
    - `text = lastPrompt + "\n" + extractText(event.message)`；缺 user prompt 时仍保留 assistant 文本，并在 metadata 标 `prompt_missing: true`；
    - 空/纯空白跳过；>64KB 截断；过短（如只有 “ok”）仍保留，但可降低 importance，避免误删真实短指令；
    - `enqueueTurn`（默认 2s 超时足够）；同一 `(session_id, turn_index)` 重复入队用 `ON CONFLICT DO NOTHING`，并记录 `turn_id`（若 Pi 提供）用于诊断；
    - `void drainPendingTurns(sessionId)`；
  - `drainPendingTurns`：
    - 循环 `claimPendingTurn`，每次只处理一个；没有可 claim 就退出；不依赖进程内“串行”标志；
    - `ensurePrompts` → LLM → `retainTurnMemories`（llmResult）→ `completePendingTurn(owner)`；
    - 失败 `failPendingTurn(owner)`，带退避；下次 `session_start`/`turn_end` 重试；
    - `retainTurnMemories` 返回 `snapshot_changed` 时：**不能用旧 `llm_result` 重试**；重新读取当前 working 文本（working 存在且比 pending 新时以 working 为准，否则用 pending 文本），重新调用 LLM 后再 retain；最多 2 次，仍冲突则 `failPendingTurn` 记录错误，等待下次重试；
    - 如果 retain 成功但 complete 失败：lease 过期后重试会再次调用 LLM，但 retain 幂等；可接受（at-least-once）；也可在 retain 成功后先写 `last_retain_at`，重试时跳过 LLM 直接 complete；
  - `session_start`：drain 一次；
  - `turn_end` 的 outbox drain 完成后检查 count/dirty，达到阈值立即安排后台 consolidation；
  - `session_shutdown`：最多等 2s drain，剩余留给下次启动；
- 保留同步 `retainTurnMemories` 路径，供测试/命令使用。

**测试**：

- `turn_end` 在 LLM 延迟 5s 时仍 <100ms 返回；
- 两个 worker 同时 claim 同一 pending turn → 只有一个成功，另一个拿到 `None`；
- worker 崩溃后 lease 过期，另一个 worker 能 claim；
- pending 在“重启”后仍存在并能 drain；
- LLM 失败 → 规则路径仍写入（`retainTurnMemories` 的 fallback 逻辑）；
- user prompt + assistant 文本都进入 retain 文本；
- 同一 turn 重复 enqueue/drain 不重复处理；lease 过期导致的重复 LLM 调用不产生重复 structured 行；
- retain 返回 `snapshot_changed` 时，worker 重新读取最新文本并重跑 LLM，旧结果不落库；两次仍冲突则进入 backoff，不丢 pending；
- 两个 subagent 使用不同 session id，`turnIndex` 相同也不冲突；
- 缺少 sessionManager 时回退 id 每进程唯一，不再共享 `"default-session"`；
- 缺 user prompt 的 turn 仍保留 assistant 文本并标 `prompt_missing`。

**验收**：`lifecycle.test.ts` 增加“慢 LLM 不阻塞”“两 worker 竞争 claim”“lease 过期恢复”“outbox 重放”。

#### PR-3.4 Compaction hooks

**目标**：compaction 前后不丢未持久化记忆。

**改动**：

- `pi.on("session_before_compact")`：
  - 对当前 session 做一次有界 drain（≤1.5s）；
  - 不发起长 LLM consolidation；必要时只走规则 retain；
- `pi.on("session_compact")`：
  - 记录 `compaction` 事件（timeline 或 session_state metadata）：`reason`、`summary` hash、`firstKeptEntryId`；
  - 可选：重置 `has_recalled_for_first_turn`，让下一轮重新召回；
- `pi.on("session_compact_failed")`：记录日志，不影响主流程；
- 使用 Pi 当前版本的类型（`SessionBeforeCompactEvent` / `SessionCompactEvent`）。

**测试**：

- before_compact 会 drain pending；
- compact 后 session_state 有 checkpoint；
- compaction 失败不阻塞 session。

**验收**：`lifecycle.test.ts` 覆盖。

#### PR-3.5 Consolidation 自动触发（P1）

**目标**：episodic 不再依赖“用户优雅退出并点确认”这一个路径。

**当前行为**：`ts/index.ts:59-71` 只在 `event.reason === "quit"` 且 `ctx.ui.confirm` 返回 true 时 consolidation；headless、崩溃、直接关窗口、用户点“否”都不会产生 episodic。

**改动**：

- 触发条件（满足任一）：
  - 当前 session 未 consolidate 的 working 达到阈值（默认 20 轮，`PI_MEMORY_CONSOLIDATE_EVERY=20`）；
  - 项目级 `consolidation_dirty=true`（已 consolidated working 被更新后需要重算）；
  - `session_compact` 之后（由 PR-3.4 调用）；
  - 显式 `/memory consolidate`；
  - `session_shutdown` 作为兜底，但**不再要求用户确认**；确认只用于删除类操作；
- 执行方式：
  - 后台任务，不阻塞 `turn_end`；复用 PR-1.3 的 claim/heartbeat/commit RPC + stable id，保证并发/重试只产生 1 条；LLM 摘要同样在事务外；
  - LLM 可用时生成摘要，不可用/超时走规则拼接并标 `degraded=true`；
  - 失败按 session 指数退避（2s 起步、最长 5m），不无限重试；手动命令可绕过冷却；`diagnose` 显示本进程上次尝试、连续失败数、下次重试、失败原因和待 consolidate working 数；
  - consolidation LLM 输入单独限额（`PI_MEMORY_CONSOLIDATE_MAX_INPUT_BYTES`，默认 64 KiB）；超限不截断工作记忆，而是跳过 LLM、用完整内容生成规则摘要并标记 degraded，诊断记录预算跳过次数；
  - 单 session 同时只允许一个 consolidation job；跨进程用 lease/claim；
- 用户控制：
  - `PI_MEMORY_AUTO_CONSOLIDATE=0` 关闭自动，仅保留手动命令；
  - 可选 `/memory consolidate off` 对当前 session 关闭，状态写入 `session_state.metadata`；
- 成本控制：自动 consolidation 使用与 turn extraction 相同的 LLM 超时/重试策略，但独立预算；每 N 轮才调用一次，避免每轮都摘要。

**测试**：

- 达到阈值自动产生 1 条 episodic，无需 quit/confirm；
- headless（无 UI）仍能 consolidation；
- 两个 worker 同时触发 → 1 条 episodic；
- LLM 失败 → 规则路径产生 episodic，`degraded=true`；
- `PI_MEMORY_AUTO_CONSOLIDATE=0` 时不自动执行；
- consolidation 后 consolidated working 从召回排除（PR-2.4）。

**验收**：`lifecycle.test.ts` 增加 headless/阈值/并发/关闭开关用例；`diagnose` 能看到待 consolidate 数。

### 5.6 M4：质量、评估、维护（P2）

#### PR-4.1 Token-aware recall + 近似去重/配额

- 注入前按 token/字符预算选择（先启发式：CJK 1 字≈1 token，英文 4 字符≈1 token；上限 800–1200）；
- 每类配额：working ≤3、episodic ≤2、structured ≤4；
- 近似去重：PR-2.4 已做“内容 hash 完全相同 + `summary_of` 关联 working”的精确去重；本 PR 增加 near-duplicate（归一化后编辑距离/SimHash）与同主题配额；
- `diagnose` 输出候选数/注入 token/丢弃原因（阈值、去重、配额、预算）。

#### PR-4.2 评估与成本指标

- `eval/recall.jsonl`：query、relevant IDs、language、kind；
- `pi-memory-eval`：Recall@5/10、MRR、nDCG；FTS-only / vector-only / hybrid 三种模式；
- 修 `pi-memory-bench`：当前 query 是英文 “how about X?” 对中文内容，FTS 往往为空，benchmark 没测到 hybrid；改为中英文 query 集，输出候选数/命中来源；
- 成本指标：每 100 turn 的 LLM 调用/token、outbox 深度、degraded 比例、注入记忆数；
- CI：无模型时用 `FakeEmbedder` 跑确定性测试；有模型时 nightly/手动跑质量评估。

#### PR-4.3 清理、过期与文档

- `/memory clear` 与迁移 scope 已在 PR-2.6 完成；本 PR 只做发布说明、旧数据损失披露和清理任务；
- 过期清理：`purge_expired` 删除过期 working/episodic + 对应 embeddings + 从向量索引移除；启动/`/memory purge` 触发；
- 孤儿 embedding / `content_map` / 项目 runtime LRU 的收尾；
- 隐私与权限文档：DB 目录 0700、文件 0600、socket 0600；日志不记录记忆正文；多用户共享机器的清理建议；
- README：
  - 单例 server 架构、socket 模式、Windows 限制；
  - 中文检索说明、模型说明；
  - benchmark 方法论与真实数字；
  - 删除不存在的 “FTS blind-spot 测试” 描述；
  - 明确 `clear`/`migrate` 的 scope 语义；
- `docs/RELEASING.md` 增加迁移/回滚说明。

---

## 6. 文件级改动清单

| 文件 | 主要改动 | PR |
|---|---|---|
| `crates/core/src/ids.rs`（新） | 内容寻址 ID、规范化、SHA-256 | 1.1 |
| `crates/core/src/text.rs`（新） | normalize、terms、search_text、FTS query | 1.7 / 2.1 |
| `crates/core/src/embedding_provider.rs`（新，可选） | `EmbeddingProvider` trait + FakeEmbedder | 1.4 / 2.2 |
| `crates/core/src/runtime.rs` | ID 赋值、retain 事务、consolidation、recall 管线、structured terms、clear | 1.1–2.6 |
| `crates/core/src/storage/turso.rs` | search_text、FTS SQL、tx/claim lease、structured terms、clear/迁移 scope/partial 状态、missing embeddings | 1.2–4.3 |
| `crates/core/src/embedding_runtime.rs` | model filter、hydrate reset、batch upsert、backfill、async load、content_map 上限 | 1.4 / 3.1 |
| `crates/core/src/embedding.rs` | padding/truncation/special tokens、动态模型参数 | 1.4 / 2.5 |
| `crates/core/src/model_download.rs` | 文件校验和、原子下载、失败退避元数据 | 1.4 |
| `crates/core/src/model_manifest.rs`（新） | 版本化 manifest（revision/size/sha256）、发布校验 | 1.4 |
| `crates/core/src/core/recall.rs` | RRF、HashMap 聚合、阈值、配额/去重接口 | 1.6 / 2.2 / 4.1 |
| `crates/core/src/core/consolidation.rs` | 稳定 ID 输入、summary_of 语义、有界合并 | 1.3 |
| `crates/core/src/core/extraction.rs` | 真实时间戳、来源字段 | 1.5 |
| `crates/core/src/models.rs` | 如需 `search_text` 字段（或只存 DB 不暴露） | 1.7 |
| `crates/core/src/bin/pi-memory-server.rs` | per-project lock、socket lock、prepareModel 异步、migrate hydrate/scope、poison/conn_count、stats | 1.4 / 2.6 / 3.1 |
| `crates/core/src/bin/pi-memory-bench.rs` | 真实 query 集、候选/来源统计 | 4.2 |
| `crates/core/src/bin/pi-memory-eval.rs`（新） | Recall/MRR/nDCG | 4.2 |
| `crates/core/tests/*` | 新增/更新 P0/P1 回归 | 全程 |
| `ts/index.ts` | consolidation claim/commit RPC、prompt 捕获、outbox drain、compaction hooks、超时分级、session 标识 | 1.3 / 3.2–3.5 |
| `ts/src/client.ts` | 方法超时、不销毁连接、socket detach、Windows 判断 | 1.3 / 3.1–3.2 |
| `ts/test/*` | 生命周期/超时/并发/outbox | 全程 |
| `docs/REVIEW.md` / `docs/REPAIR_PLAN.md` | 审阅证据、计划、迁移/回滚 | 全程 |
| `README.md` / `docs/*` | 架构、迁移、benchmark、限制 | 4.3 |

---

## 7. 测试矩阵

| # | 测试 | 层级 | 关键断言 | 优先级 |
|---|---|---|---|---|
| 1 | `retain_llm_multiple_items_persist_all` | Rust integration | 3 facts → 3 行；四类各自成立 | P0 |
| 2 | `retain_retry_is_idempotent` | Rust integration | 重试后行数不变 | P0 |
| 3 | `retain_repairs_legacy_partial_turn` | Rust integration | 规则可提取项补齐并标 `rule_complete`；LLM-only 项标不可恢复 | P0 |
| 4 | `retain_failure_rolls_back` | Rust (feature) | working 不残留 | P0 |
| 5 | `consolidate_retry_single_episodic` | Rust integration | 两次调用 → 1 条 | P0 |
| 6 | `consolidate_concurrent_lease` | Rust integration | 两个 worker 对同一集合 → 1 个 lease/1 条；部分重叠见 #41/#42 | P0 |
| 7 | `consolidate_commit_failure_rolls_back` | Rust (feature) | episodic 插入失败 → `consolidated_at` 仍空、lease 释放 | P0 |
| 8 | `episodic_embedding_written` | Rust integration | embedding 行 + index_len | P0 |
| 9 | `embedding_backfill_closes_gap` | Rust integration | missing=0 | P0 |
| 10 | `embedding_model_filter` | Rust integration | 旧 model 不加载 | P1 |
| 11 | `batch_embedding_no_orphans` | Rust unit | 重复回填 index_len 不变 | P1 |
| 12 | `fts_cjk_substring_hits` | Rust integration | “向量”命中 | P0 |
| 13 | `fts_topk_bm25_limit` | Rust integration | ≤K 条、分数非百分位 | P0 |
| 14 | `hybrid_vector_outside_fts` | Rust integration (Fake) | 最终 Top-6 含 vector 命中 (`hybrid_recall.rs`) | P0 |
| 15 | `vector_topk_when_fts_empty` | Rust integration (Fake) | 返回索引中全部 Top-K 候选，不是 1 条 (`hybrid_recall.rs`) | P0 |
| 16 | `structured_long_prompt_hits` | Rust integration | 长 prompt 命中 fact、inactive instruction 不召回 (`llm_integration.rs`) | P0 |
| 17 | `metadata_filters` | Rust integration | 过期/superseded/inactive 不召回 | P1 |
| 18 | `socket_cold_start_single_server` | Rust process integration (`socket_server.rs`) | 双进程冷启动仅一进程存活；权限 0700/0600；shutdown 后清理 socket（受限沙箱拒绝 bind 时显式跳过） | P1 |
| 19 | `concurrent_projects_not_blocked` | Rust server unit (per-project lock) | 项目 A 持锁时，项目 B 可并发进入 runtime (`busy_project_runtime_does_not_block_another_project`) | P1 |
| 20 | `timeout_does_not_destroy_connection` | TS unit (`timeout.test.ts`) | 一个 socket call 超时后其他 pending 仍成功；迟到响应被忽略；stdio 超时不 kill 进程 | P1 |
| 21 | `turn_end_fast_with_slow_llm` | TS lifecycle | <100ms 返回 | P1 |
| 22 | `pending_turns_replay` | Rust storage reopen + TS lifecycle (`pending_turn_outbox_survives_storage_reopen_and_reclaims_expired_claim`, `session_start replays a durable pending turn after restart`) | DB 重开后 pending 与过期 lease 仍可恢复；`session_start` 重新 claim、retain 并完成 | P1 |
| 23 | `compaction_flushes_pending` | TS lifecycle | before_compact drain | P2 |
| 24 | `recall_metrics_eval` | eval binary + nightly workflow | default 40-case and cross-lingual semantic 40-case fixtures report Recall@5/10, MRR, nDCG, latency by language; `--explain-misses` attributes target losses to source ranks and filters | P2 |
| 25 | `clear_scope_semantics` | Rust/TS | target 不同时不误清当前；all 需确认 | P0 |
| 26 | `outbox_claim_lease` | Rust integration | 两 worker 只有一个 claim；lease 过期可恢复 | P1 |
| 27 | `fts_v2_switch_preserves_v1` | Rust integration | v2 创建/切换失败时 v1 仍可查；状态不脏 | P0 |
| 28 | `vector_upsert_failure_keeps_old` | Rust unit | add 失败旧映射可搜；rehydrate 恢复 | P1 |
| 29 | `structured_timestamp_not_epoch` | Rust integration | 规则/LLM 路径均为 turn 时间 | P0 |
| 30 | `migrate_scope_filter` | Rust integration | 只导入目标 scope；源库不被改写 | P0 |
| 31 | `model_checksum_rejects_corrupt` | Rust unit | 与 manifest 的 size+SHA-256 比对；不匹配不进入 Ready；pinned revision | P1 |
| 32 | `model_failed_retry_backoff` | Rust unit | 请求路径不重试；后台按退避重试 | P1 |
| 33 | `embed_batch_mixed_lengths` | Rust unit | 不同长度 batch 成功；超长截断 | P1 |
| 34 | `poison_and_conn_count_recovery` | Rust server unit (`connection_count_guard_decrements_during_panic_unwind`, `poisoned_project_runtime_recovers_and_rehydrates`) | panic unwind 后连接计数归零；runtime 锁清 poison、回滚并重建向量索引后可继续使用 | P1 |
| 35 | `content_map_bounded` | Rust runtime design | `ProjectVectors` 不缓存记忆正文，只保存 `(kind,id)` 映射并按需从 DB 加载候选；跨项目 runtime 由容量受限 LRU 驱逐 | P2 |
| 36 | `auto_consolidation_headless` | TS lifecycle | headless 下 `turn_end` drain 后达到阈值会触发 consolidation；不依赖 quit/UI | P1 |
| 37 | `consolidated_working_not_recalled` | Rust integration | working 与 episodic 不重复注入 | P1 |
| 38 | `recall_min_score_drops_irrelevant` | Rust unit | 全不达标时注入为空 | P1 |
| 39 | `recall_merge_no_quadratic` | Rust bench (`pi-memory-bench --merge-only`) | 3×50 候选 RRF 合并输出 P50/P95/max，P95 对照 5ms 门槛 | P2 |
| 40 | `session_fallback_unique` | TS lifecycle (`missing sessionManager uses a stable process-scoped fallback ID`, `sessionManager ID takes priority over PI_SESSION_ID`, `blank sessionManager ID falls back to PI_SESSION_ID`) | manager 优先；空值继续查 env；都缺失时使用当前进程稳定且非默认值的随机 ID | P1 |
| 41 | `consolidate_partial_claim_no_stall` | Rust integration | 输入含已 consolidated 行时不整体回滚；只处理未处理子集 | P0 |
| 42 | `consolidate_disjoint_claims_cover_all` | Rust integration | 两 worker 子集不相交时覆盖全部 working，无重复 claim | P0 |
| 43 | `retain_same_turn_text_change_replaces` | Rust integration | 同 ID 新文本替换 structured，旧行不残留，source_sha 更新 | P0 |
| 44 | `retain_same_hash_skips_rewrite` | Rust integration | 同 ID 同 hash 不重写 structured/embedding | P1 |
| 45 | `model_manifest_pinned_revision` | Rust unit | 下载 URL 固定 revision；hash/size 与 manifest 比对 | P1 |
| 46 | `model_manifest_custom_model` | Rust unit | 自定义模型需 manifest；`VERIFY=0` 仅 dev 放行 | P2 |
| 47 | `consolidate_no_llm_inside_tx` | Rust integration | 摘要回调期间 `tx_depth()==0`；lease 已提交 | P0 |
| 48 | `consolidate_lease_expiry_recovery` | Rust integration | 崩溃后 lease 过期可重 claim；只产生 1 条 | P0 |
| 49 | `consolidate_snapshot_changed` | Rust integration | 摘要期间内容变化 → 提交失败、working 仍可重试 | P0 |
| 50 | `consolidate_summary_of_equals_claim` | Rust integration | `summary_of == claimed_working_ids` 集合严格相等 | P0 |
| 51 | `fts_migration_single_leader` | Rust integration | 两实例并发启动只有一个 leader；另一个不阻塞查询 | P0 |
| 52 | `fts_migration_crash_recovery` | Rust (feature) | 回填/建索引/切换中 kill 后状态可恢复且读者一致 | P0 |
| 53 | `clear_index_partial_state` | Rust integration | DB 已删、索引重建失败 → `index_state=partial`，rebuild 后一致 | P1 |
| 54 | `migrate_index_partial_state` | Rust integration | 导入成功、索引失败 → 数据可读、`partial`、不重复导入 | P1 |
| 55 | `model_verify_disabled_release_rejected` | Rust unit | release 构建禁用 `VERIFY=0` 并报配置错误；维度/tokenizer 仍校验 | P1 |
| 56 | `consolidate_claim_commit_rpc` | Rust/TS integration | claim → LLM/heartbeat → commit；集合严格一致 | P0 |
| 57 | `consolidate_heartbeat_expiry` | Rust/TS integration | 停止心跳后 lease 过期，旧 token commit 被拒，新 owner 可完成 | P0 |
| 58 | `consolidate_token_invalid_after_restart` | Rust/TS integration | server 重启后旧 token `CLAIM_EXPIRED`；不会误提交其他行 | P1 |
| 59 | `retain_snapshot_conflict_retries` | Rust unit（第二 DB connection）+ TS integration | 提取期间同 turn 文本更新，旧 snapshot 不提交；Rust 明确返回可重试错误，TS outbox 重新读取并重跑 LLM | P0 |
| 60 | `fts_expired_leader_fenced` | Rust integration | 新 leader 接管后旧 fencing token 的回填写入/phase 更新/状态切换全部被拒绝；旧 leader 的幂等建索引不影响最终状态 | P0 |
| 61 | `clear_dirty_state_survives_restart` | Rust integration | 删除事务提交后进程重启仍识别 dirty 状态，不从旧向量索引召回 | P1 |
| 62 | `old_server_write_then_new_server_rebuilds_fts` | Rust integration | 旧版写入导致 `search_text` 为 NULL；新版先回退 v1 保证召回，再回填并安全切回 v2 | P1 |
| 63 | `compaction_failure_is_nonblocking` | TS lifecycle (`session_compact_failed records the failure without disrupting the session`) | compact 失败 hook 不抛异常、不发 RPC，主会话可继续 | P2 |
| 64 | `consolidated_working_update_reconsolidates` | Rust integration | 更新已 consolidated working → 旧 episodic 失效、整组重新 claim；新 episodic 覆盖最新内容 | P0 |
| 65 | `migrate_crash_before_rebuild_dirty` | Rust (feature) | 导入事务提交后崩溃 → 重启仍识别 `index_state=partial`，不信任空索引，重建后恢复 | P0 |
| 66 | `clear_all_dirty_scope_isolation` | Rust integration | `clear all` 逐 scope 重建时，一个 scope 失败不会清除其他 scope 的 dirty 标记 | P1 |
| 67 | `outbox_snapshot_conflict_retries` | TS integration | `snapshot_changed` 后重读最新文本并重跑 LLM，旧结果不落库；耗尽后 pending 保留 | P1 |
| 68 | `fts_migration_heartbeat_survives_default_production_lease` | Rust process integration | 默认 30 秒 TTL 下回填持续超过原始租约；心跳续约有效、第二实例不能接管，释放后迁移切换成功 | P0 |
| 69 | `structured_recall_10k_gate` | Rust benchmark (`pi-memory-bench --assert-gates`) | 每张 structured 表至少 10k 行；structured 查询 P95 ≤20ms、端到端 recall P95 ≤100ms，否则返回失败状态 | P0 |
| 70 | `fts_query_filters_english_stop_words` | Rust unit/storage | 英文函数词不进入 FTS OR 查询；只含函数词时返回空候选；CJK bigram 保持原样 | P1 |
| 71 | `recall_budget_dedup_and_quota` | Rust unit (`runtime::tests`) | 验证 CJK/英文 token 估算、预算上限和超额丢弃、规范化近似去重、主题相似度过滤与类别配额诊断 | P2 |
| 72 | `purge_expired_scope_and_vector_consistency` | Rust storage integration (`turso_storage.rs`) | 只清理目标 scope 的过期/superseded working/episodic，并同步删除 embeddings；清孤儿 embedding 后重建向量索引，索引规模与剩余有效记忆一致 | P1 |
| 73 | `diagnose_reports_operational_cost_metrics` | TS lifecycle (`lifecycle.test.ts`) | 完成一次带 recall 与 LLM 的 turn 后，`/memory diagnose` 输出每百 turn LLM 调用/估算 token/注入记忆数、outbox 深度和 consolidation pending 数 | P2 |
| 74 | `private_project_database_permissions` | Rust Unix integration (`turso_storage.rs`) | 项目数据目录各级为 `0700`，主数据库为 `0600`；SQLite sidecar 受私有目录保护 | P1 |
| 75 | `project_runtime_lru_skips_shared_runtimes` | Rust server unit (`pi-memory-server`) | 超限时保留当前/仍被引用 runtime，按 `last_access` 驱逐最老 idle 项目；引用释放后可被驱逐 | P2 |
| 76 | `retain_repairs_legacy_partial_turn` | Rust integration (`llm_integration.rs`) | 旧 `partial` turn 从 working 内容重放可恢复规则项，删除旧 LLM-only 结构化项，并迁移为 `rule_complete` | P0 |

**测试数据隔离**：所有 Rust 集成测试使用临时目录 + 独立 DB；embedding 相关测试优先用 `FakeEmbedder`，真实模型测试标 `#[ignore]`，CI 不依赖模型下载。

---

## 8. 迁移、兼容与发布

### 8.1 Schema 版本与 FTS 安全切换

新增 `app_meta(key TEXT PRIMARY KEY, value TEXT)`，至少记录：

- `schema_version`：整数；
- `fts_state`：`content` | `search_text`（当前查询走哪套索引）；
- `fts_version`：`1`（content）| `2`（search_text）；
- `fts_phase`：`idle` | `backfilling` | `backfilled` | `index_ready` | `search_text`；
- `fts_last_error`：最近一次迁移错误（脱敏）；
- `fts_dirty` / `needs_rehydrate` / `index_state`：跨重启持久化的索引恢复标记；单 DB 多 scope 时按 scope 记录（`<key>:<scope>`）或使用 per-scope 状态表；
- `consolidation_dirty`：已 consolidated working 更新后需要后台重算的标记；
- `embedding_model`：当前默认 model id。

迁移锁单独建表，避免把 owner 过期判断塞进 key-value：

```sql
CREATE TABLE IF NOT EXISTS fts_migration_lock (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  owner TEXT,
  expires_at TEXT,
  acquired_at TEXT,
  fencing_token INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO fts_migration_lock(id) VALUES (1);
```

`owner` / `expires_at` / `acquired_at` 使用 UTC ISO-8601 字符串，保证字典序比较与时间顺序一致；另有单调递增的 `fencing_token`，每次成功 acquire 都递增；锁只保护迁移，不参与普通查询。

`search_text` 用**可空列**（不是 `NOT NULL DEFAULT ''`），这样能区分“未回填”和“原文为空”：回填条件用 `WHERE search_text IS NULL`。

**多实例规则（必须）**：迁移只允许一个 leader 执行；所有实例的查询都不等锁，也不因为别人迁移而阻塞。

- **获取 lease**：在短事务里条件更新 owner/expiry 并递增 fencing token；只有 `changes()==1` 才算 leader。TTL 默认 30s。expiry 到期允许竞争者尝试接管；原 owner 可在 token 未变化时续期，续期与接管由条件更新原子仲裁。每次成功接管都获得更大的 token，旧 leader 的 token 永远不能重新生效；旧锁表需按启动迁移步骤幂等补上该列。
- **心跳与写入 fencing**：leader 每 10s 续期；续期和每批回填都校验 owner/token，不能“先检查、之后单独写”，避免旧 leader 暂停后在新 leader 接管后继续写。**状态切换必须在持有当前 token 的事务内校验**；`CREATE INDEX IF NOT EXISTS` 可重复执行，旧 leader 创建同名索引本身无害，但旧 leader **不能设置 `fts_phase` 或切换 `fts_state`**；创建后必须重新校验 token，再进入 `index_ready`。owner/token 不再匹配的实例立即停止。
- **非 leader**：直接按当前 `fts_state` 提供查询；记录 `diagnose` 信息（当前 owner/expiry），不执行任何回填或 DDL。
- **leader 崩溃**：lease 到期后，下一次启动或定期检查的实例可接管；所有步骤必须可重入，从 `fts_phase` / `search_text IS NULL` 的实际状态继续。
- **读者一致性**：`fts_state` 只在“v2 索引已创建且通过探测查询”之后切换；切换是单条小事务。读者在一次查询开始时读取 `fts_state`，因此只会看到切换前或切换后的完整状态，不会看到“状态已切、索引还没建”的中间态。`fts_state` 可以在进程内缓存，但必须在每次 recall 前校验版本（例如 `app_meta.fts_version`）或短 TTL 刷新；不能只在启动时读一次，否则另一个实例切换后本实例会一直走旧索引。

**启动/打开项目时按以下顺序执行（每一步幂等）**：

0. `CREATE TABLE IF NOT EXISTS app_meta...` + `fts_migration_lock`；旧版已存在锁表但无 `fencing_token` 时幂等补列并设为 `0`；没有 `fts_state` 时写入 `content`（老库默认）；没有 `fts_phase` 时写入 `idle`。
0a. **版本回滚后的完整性检查**：新 server 开始对项目提供 recall 前，检查 `fts_state=search_text` 时 `search_text IS NULL` 的行数。若非零（例如旧 server 在回滚期间写入了不认识 `search_text` 的行），先原子地将 `fts_state` 切回 `content`、设置 `fts_dirty=true` / `fts_phase=backfilled`，再提供查询；不能在 NULL 未回填时继续只查 v2。随后由迁移 leader 回填 NULL、验证 v2、再切回 `search_text`。检查应在服务开放查询前完成；迁移中的常规新版本实例仍遵守单 leader 规则。
1. `ALTER TABLE working_memory ADD COLUMN search_text TEXT`；episodic 同理；已有列则跳过。两个实例可能同时做这一步：先查 `PRAGMA table_info`，并把 “duplicate column” 错误视为已存在，不要把它当成迁移失败。旧 FTS 仍索引 `content`，`fts_state` 保持 `content`。
2. **尝试获取迁移 lease**。失败则跳过迁移，继续按 `fts_state` 查询；不要并发回填、不要并发建索引。
3. leader 分批回填：`WHERE search_text IS NULL`，每批最多 100 行，写入 `build_search_text(content)`；空内容写 `''`。较小批次缩短写事务并让续期更快持久化；旧索引和旧查询路径不受影响；每批前检查 lease 并心跳。`fts_phase=backfilling`。
4. 校验：`SELECT COUNT(*) WHERE search_text IS NULL` 必须为 0；未完成则保持 `backfilling`，下次接管继续，不切换。完成后 `fts_phase=backfilled`。
5. **创建新索引，但保留旧索引**：
   ```sql
   CREATE INDEX IF NOT EXISTS fts_working_idx_v2 ON working_memory USING fts (search_text);
   CREATE INDEX IF NOT EXISTS fts_episodes_idx_v2 ON episodic_memory USING fts (search_text);
   ```
   创建后用一条真实查询探测 v2 可用（例如 `SELECT ... WHERE fts_match(search_text, ?) LIMIT 1`，允许 0 行但不得报错）。创建/探测失败：旧索引仍在，`fts_state=content`，`fts_phase` 保持 `backfilled` 或 `index_ready` 前的状态；记录 `fts_last_error`，下次重试。
6. 重新校验 lease/fencing token；设置 `fts_phase=index_ready`（仅在 v2 探测成功后），然后**在单个小事务内切换状态**，并在事务内再校验一次：
   ```sql
   BEGIN;
   -- 重新检查 owner/expiry、search_text IS NULL = 0、fts_phase=index_ready（v2 探测已通过）
   UPDATE app_meta SET value='search_text' WHERE key='fts_state';
   UPDATE app_meta SET value='2' WHERE key='fts_version';
   UPDATE app_meta SET value='search_text' WHERE key='fts_phase';
   COMMIT;
   ```
   任一校验失败 → rollback，`fts_state` 保持 `content`；v2 索引可以存在但不会被使用。
7. 切换成功后仅由匹配当前 `owner + fencing_token` 的 leader 释放 lease（`owner=NULL`）；旧 leader 不得清除新 leader 的锁。查询层在每次查询开始时读 `fts_state`，选择 `content` 或 `search_text` 列；切换前开始的查询继续用 v1，切换后的查询用 v2。
8. 旧索引 `fts_working_idx` / `fts_episodes_idx` 至少保留一个发布版本，用于回滚/降级；下个大版本再删除。

**不变量（启动时和 `diagnose` 都检查）**：

- `fts_state=search_text` ⇒ `search_text` 无 NULL 行 **且** v2 索引存在且探测通过。违反则在任何 recall 前原子回退 `fts_state=content`、设置 `fts_dirty=true`，再报警并重新走迁移；不允许只在进程内回退或继续走 v2。
- `fts_state=content` ⇒ v1 索引存在（或可重建）；v2 是否存在不影响查询。旧 server 写入期间可能产生 `search_text IS NULL`，新 server 恢复后必须先执行上面的完整性检查和回填，再切回 v2。
- `fts_phase=search_text` ⇔ `fts_state=search_text`；阶段与实际状态不一致时以 `fts_state` 为准并修复 `fts_phase`。
- 迁移锁不参与查询；锁丢失只影响“谁来迁移”，不影响“读哪套索引”。

**为什么不是“先 DROP 再 CREATE”**：即使 Turso 的 DDL 可事务回滚（本次 probe 已验证 `DROP + 失败 CREATE + ROLLBACK` 后旧索引仍可查），先 DROP 仍会让“迁移中查询”和“旧代码回滚”失去旧索引。保留 v1 + 新建 v2 的方案迁移期间零中断、失败可回退，代价是过渡期多一份 FTS 索引空间。

**迁移中查询如何降级**：`fts_state=content` 时新代码仍走旧的 `content` 查询（中文效果差但能用）；`fts_state=search_text` 且 v2 索引存在时才走新路径。若 v2 索引缺失但状态已切换，`diagnose` 报警并回退到 `content` 路径，同时后台 `rebuild-fts`。

**双实例/崩溃测试（新增）**：

- 两个进程同时打开同一个旧 DB：只有一个获得 `fts_migration_lock`；另一个不执行回填/DDL，按 `content` 正常查询；
- leader 在回填到一半时被 kill：`search_text` 有部分 NULL，`fts_state` 仍 `content`；下一个实例接管后从 `IS NULL` 继续，最终切换；
- 旧 leader 在 lease 过期后恢复：旧 fencing token 的回填/建索引/状态切换事务全部被拒绝，新 leader 的结果不被旧实例覆盖；
- leader 在 `CREATE INDEX` 后、切换状态前被 kill：v2 可能存在，`fts_state` 仍 `content`；接管后 `IF NOT EXISTS` + 探测通过再切换；
- leader 在状态切换事务中被 kill：读者只看到 `content` 或 `search_text` 之一；两种状态下查询都成功；
- `fts_state=search_text` 但人为删除 v2 索引：启动时检测到不变量破坏，原子回退 `content` + `fts_dirty=true`，不返回空结果；
- 模拟旧 server 在 v2 状态下写入未填 `search_text` 的 working/episodic 行后退出：新 server 开放 recall 前回退到 v1，查询能命中新记录；回填后切回 v2，查询结果仍完整；
- 迁移期间另一个实例执行 recall：不阻塞、不报错，结果来自当前 `fts_state` 对应的索引。


**本次 probe 结果**（临时文件已删除）：

- `DROP INDEX + 失败 CREATE INDEX + ROLLBACK` 后旧 FTS 索引仍可查询；
- `DROP + CREATE + COMMIT` 后新索引可查询；
- 同一表上同时存在 `fts(content)` 与 `fts(search_text)` 两个索引时，两种查询都能命中。因此 v1/v2 并存可行；
- 已有 v1 FTS 索引时，`ALTER TABLE ADD COLUMN search_text` + 回填 + 建 v2 索引可正常工作。

**验证状态**：两个独立进程的迁移与崩溃恢复由 `fts_migration_crash_recovers_across_processes` 覆盖：回填批次超过 3 秒测试 TTL，另一连接确认 expiry 已续到未来且第二实例不能成为 leader；读者在回填及 v2 索引已创建、状态尚未切换时仍从 content 索引召回；分别杀死 leader 并等待 lease 过期后，新 leader 均能接管并完成切换。新增 `fts_migration_heartbeat_survives_default_production_lease` 使用默认 30 秒 TTL，跨进程回填持续超过该 TTL，验证 lease 仍有效、第二实例不能接管，释放后迁移成功切换。单进程用例另覆盖 lease takeover fencing 与迁移阶段续跑。

**适用范围**：这些测试覆盖项目当前使用的 `turso_core` embedded engine 与条件更新语义；若未来接入远程 Turso/libSQL client，应对远程事务/changes 行为单独做 integration probe，再决定是否需 `SELECT ... FOR UPDATE` 等价方案。

### 8.2 旧数据

- **已丢失的多条 fact**：旧版 LLM 路径在写入时已经覆盖，无法从 DB 恢复；只能在 release notes 说明，并建议用户对重要 turn 重新 retain（或未来提供 `re-extract` 命令）。
- **旧版 structured ID**（`:fact:llm`）：retain 时按 `source_memory_id` 删除并重建；一次性 `repair-legacy` 命令可批量处理；自动修复只做规则重放，LLM-only 项标 `legacy_llm_unverified`，需要 `/memory reextract` 显式再调用 LLM。
- **旧 embedding**：按 `model` 过滤；未标记/旧模型的行不加载，`rebuild-embeddings` 重建。
- **旧时间戳（1970）**：不自动改写所有行（无法区分“真实未知时间”和旧 bug）；对能通过 `source_memory_id` 关联到 working 的 structured 行，`repair-legacy` 用 working.`created_at` 回填；其余保持 1970 并在 `diagnose` 计数。
- **旧 `source_sha` 缺失**：旧行没有 hash，不能证明“文本未变”。第一次遇到时按“文本更新”处理：删除旧 structured，用 `working.content` 重放规则提取并写入新 `source_sha`；标记 `rule_complete`，不标记 `llm_complete`；LLM-only 项由 `diagnose` 标 `legacy_llm_unverified`，需要 `/memory reextract`。
- **旧 FTS 索引**：v1 保留一个版本，新建 v2（`search_text`）；迁移由 `fts_migration_lock` 的单 leader 执行，`fts_state` 切换前查询走旧索引，切换后走 v2；`search_text` 未回填完或 v2 未探测通过不切换，因此不会出现“新索引缺行”的静默漏召回。多实例启动时非 leader 不做迁移，只按当前 `fts_state` 查询。
- **迁移的 scope**：旧 DB 可能包含多个 project scope。`migrate` 必须指定目标 scope（默认当前 project），只导入匹配行；跳过的 scope/行数写入迁移报告，不静默丢弃。源库只读打开，导入事务前后校验计数；FTS/向量重建在事务外，失败以 `index_state=partial` 返回。

### 8.3 兼容性

- RPC 新字段全部 `serde(default)`；旧 client 调用新 server 不受影响；
- 新 client 调用旧 server：`enqueueTurn` / `claimConsolidation` 等新方法返回 `METHOD_NOT_FOUND`，client 回退到同步 `retainTurnMemories` / legacy consolidation；
- 旧 client 调用新 server：`consolidateSessionMemories(llm_result)` 默认返回 `SNAPSHOT_REQUIRED`（提示先 claim），只有 `PI_MEMORY_LEGACY_CONSOLIDATION=1` 时才走旧语义；`consolidateSource` 保留但标记 deprecated；
- 旧 server 代码连新 DB：v0.2 保留 v1 `fts(content)` 索引，旧代码仍可读写；旧 server 不维护 `search_text`，因此回滚期间写入会留下 NULL。版本切换必须先停止旧 server 实例，禁止新旧 server 同时写同一 DB；恢复新版后，在开放 recall 前执行 8.1 的 NULL 检查，原子回退到 v1、补齐 `search_text` 并重建/验证 v2 后再切回。v0.3 删除 v1 后不再支持降级到 v0.2 之前。
- `ConsolidateResult` 新增字段（`claimed_working_ids` / `partial` / `remaining_unconsolidated` / `contended` / `snapshot_changed` / `episodic_id`）用默认值，旧测试不破；`RetainResult` 新增 `updated` / `source_sha` / `snapshot_changed` / `current_source_sha` 同样用默认值；
- `clear` / `migrate` 的结果新增 `db_committed` / `index_state` / `fts_dirty` / `needs_rehydrate`，旧 client 可忽略；旧 client 只看到 `ok` 时按“DB 已提交、索引可能待重建”理解；
- 行为等价性：从 RRF 开始不再 bit-exact；README 的 “behavior-equivalent” 改为 “semantics-compatible with documented differences”。

### 8.4 发布与回滚

建议按三个可独立发布的版本：

1. **v0.1.17（止血）**：PR-1.1 ~ 1.5 + PR-1.7（先行）→ PR-1.6（依赖 1.7，同批发布）；修数据正确性、episodic 向量、时间戳、中文 FTS、独立向量 Top-K、模型完整性。默认排序行为尽量不变；`PI_MEMORY_RECALL_MODE=legacy` 可回退向量 allowlist，`PI_MEMORY_FTS_MODE=content` 可回退旧索引。
2. **v0.2.0（召回）**：PR-2.1、PR-2.2b ~ 2.6；完整 RRF + structured terms + 元数据/去重 + clear/迁移安全；提供 `PI_MEMORY_RECALL_MODE=legacy` 回滚一个版本。
3. **v0.2.x（可靠性）**：PR-3.x；outbox、超时、socket、自动 consolidation；提供 `PI_MEMORY_LLM_ASYNC=0` 和 `PI_MEMORY_AUTO_CONSOLIDATE=0` 回退。

回滚原则：

- schema 只增不删；回滚代码仍能读新 DB（忽略新列）；
- FTS v1 索引保留一个发布版本；v2 创建/切换失败时继续走 v1；下个大版本再删 v1；
- embedding 回滚只需切回旧 model id，旧向量仍在（不会被删）；
- outbox 回滚：未处理行保留，旧代码忽略该表。
- clear/migrate 回滚：DB 事务失败按原状态回滚；索引重建失败不回滚已提交数据，保留 `index_state=partial` 并用旧索引/DB 降级，后续 rebuild 收敛。

---

## 9. 性能与评估

### 9.1 benchmark 修复状态

早期 benchmark 的 query 与 fixture 语言不匹配，且只写 working memory，因此 FTS/vector/structured 混合路径不可观察。该缺口已在当前实现修复：`pi-memory-bench` 写入 10k working memory 及 facts/preferences/instructions/timeline 各 10k 条合成数据，轮换中英文 query，分别输出 FTS/vector/structured 候选数及端到端、structured 查询延迟。`--assert-gates` 要求每张结构化表至少 10k 行，并在 structured P95 >20ms 或端到端 recall P95 >100ms 时返回失败；2026-11-23 本地 10k 门槛运行结果与口径见 README。这仍是性能证据，不是实际项目记忆的质量评估。

### 9.2 新 benchmark 口径

| 指标 | 说明 | 目标 |
|---|---|---|
| Recall@5 / @10 | 标注集命中率 | 相对基线提升，不设绝对数直到数据集稳定 |
| MRR / nDCG@10 | 排序质量 | 记录趋势 |
| P50/P95/P99 | `buildRecallInjection` 端到端 | 10k 行 P95 ≤ 100ms（目标，先记录） |
| 候选数 | FTS/vector/structured 各自候选 | 可见、可调 |
| 注入 token | 最终注入量 | M4 后 ≤ 800–1200 |
| 写放大 | 每 turn DB 写 + FTS 更新 | 单 turn retain P95 ≤ 20ms（不含 LLM） |
| LLM 成本 | 每 100 turn 调用/token | 后台化后统计，不设硬阈值 |

### 9.3 性能注意事项

- `search_text` 会增加 DB 体积（CJK 约 2–3 倍文本），需在 10k 数据集上测；
- FTS `LIMIT` 下推依赖查询形状，PR-1.7 必须用“10k 常见词”验证，PR-2.1 用真实数据复核，不能只测小数据；
- RRF 本身 O(候选数)，可忽略；
- embedding 回填是 I/O + ONNX 密集，必须分批，不能在请求路径做全量。

---

## 10. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| Turso FTS 模式/ tokenizer 行为与预期不符 | 中文/limit 修复失败 | PR-1.7 先做 1 天 spike；`search_text` 是主方案，ngram 是备选 |
| `Connection`/`Statement` 不满足跨线程 Send/Sync | per-project 并发编译失败 | 先写 compile spike；不行则退回“每项目一个 worker 线程 + channel” |
| RRF 改变排序，老用户感知“召回变差” | 体验回退 | 保留 `PI_MEMORY_RECALL_MODE=legacy` 一个版本；用 eval 集对比 |
| `search_text` 迁移大表耗时/失败 | 启动慢/索引不可用 | 分批回填、单 leader lease、失败保留旧索引；提供 `rebuild-fts`；非 leader 不做迁移 |
| 多实例同时迁移导致重复回填/读到中间态 | 索引损坏或漏召回 | `fts_migration_lock` + heartbeat + phase；状态切换前验证 v2 可用；读者按 `fts_state` 查询 |
| Consolidation lease 过期导致重复摘要 | LLM 成本/日志噪声 | lease 180s + heartbeat；提交校验 owner/版本；同一集合 stable id；统计重复 claim |
| 多语言模型切换导致全量重嵌入 | 成本/时间 | 先评估后切换；保留旧向量；后台 `rebuild-embeddings` |
| Outbox 表膨胀 | DB 变大 | 完成/失败行定期清理；保留最近 N 天 |
| Outbox at-least-once 导致重复 LLM 调用 | 成本增加/日志噪声 | lease 默认 5 分钟、heartbeat、max attempts；retain 幂等；统计重复 claim 次数 |
| FTS v1/v2 双索引过渡期空间翻倍 | DB 变大 | 只保留一个发布版本；下个大版本 drop v1；迁移后 `diagnose` 显示两份索引大小 |
| 迁移状态 `fts_state` 与实际索引不一致 | 查询走错列/变慢 | 启动时校验 v2 索引存在 + 未回填行数为 0 + 探测查询通过；不一致则回退 `content`、置 `fts_dirty` 并告警 |
| clear/migrate 的索引重建失败 | 误报“整体失败/已回滚”，或 DB 与索引不一致 | 区分 `db_committed` 与 `index_state`；不回滚已提交数据，返回 `partial` + `needs_rehydrate`/`fts_dirty`，幂等 rebuild 收敛（PR-2.6） |
| socket 权限/同机其他用户 | 记忆泄露 | 目录 0700、socket 0600；DB 0600；日志脱敏；文档说明；未来加 peer credential 校验 |
| 模型文件损坏/半截/版本漂移 | 请求反复同步加载失败、召回全空 | 版本化 manifest 的 size+SHA-256 与 pinned revision；`VERIFY=0` 仅 dev；`Failed` 退避；请求路径不重试（PR-1.4） |
| Mutex poison / `conn_count` 泄漏 | 请求全挂、server 永不退出 | `parking_lot` 或 poison 恢复；RAII/`catch_unwind` 保证计数递减（PR-3.1） |
| `content_map` 无界增长 | 长时间运行 OOM | 项目 LRU/容量上限；召回时按需查 DB（PR-3.1） |
| `clear <target>` 误删当前项目 | 破坏性数据丢失 | 所有表按 scope 删除；target 走目标 project；all 需二次确认（PR-2.6） |
| 迁移跨 scope 混入 | 跨项目隐私泄漏/召回污染 | 迁移指定 scope；源库只读；计数校验（PR-2.6） |
| 自动 consolidation 成本/错误摘要 | token 成本、错误长期记忆 | 阈值 + 独立预算 + 可关闭；规则降级标 `degraded`（PR-3.5） |
| RRF 阈值过高导致空召回 | 用户感知“记忆变少” | eval 集校准；`PI_MEMORY_RECALL_MODE=legacy` 回退；`diagnose` 显示丢弃原因（PR-2.2b） |
| 记忆注入 prompt injection | 模型被记忆内容操纵 | M4 加分隔符/“untrusted data”提示；限制单条长度；不执行记忆中的指令 |
| Windows 路径 | 功能不可用 | 短期禁用 socket 模式 + 文档；长期 named pipe |

---

## 11. 需要确认的决策

开工前建议确认以下 10 点；未确认时按括号内默认值执行。**技术性的执行边界已在 1.6 决定，不再作为确认项**：consolidation 采用“先 claim lease、事务外摘要、短事务提交”；FTS 迁移采用单 leader lease；clear/migrate 区分 DB 事务与索引重建的 `partial` 状态。

1. **中文是否一等场景？**（默认：是。是 → PR-1.7/2.5 按 P0/P1 排；否 → 2.5 降为 P2，1.7 仍做。）
2. **是否接受放弃 bit-exact？**（默认：接受，从 RRF 开始；保留一个版本的 legacy 开关。）
3. **旧数据中已丢失的多条 fact 是否需要恢复？**（默认：不恢复，release notes 说明；只保证新写入不丢。）
4. **Windows 是否正式支持？**（默认：短期不支持 socket，回退 stdio；leaf package 标注 experimental。）
5. **是否允许新增依赖 `sha2` / `fs2` / `parking_lot`？**（默认：允许；都是小依赖，分别用于稳定 ID/校验和、socket 锁、无 poison 锁。）
6. **`turn_end` / `turnIndex` 在 subagent 之间是否唯一？**（默认：按 `(session_id, turn_index)` 唯一；session 缺失时用每进程随机 id，禁止共享 `"default-session"`；如果不唯一，改用 Pi 的 turn id。）
7. **每轮 LLM 提取的成本是否接受？**（默认：接受但后台化；若不可接受，改为规则优先 + 按重要性/长度抽样 LLM。）
8. **episodic 何时产生？**（默认：未 consolidate working 达到 20 轮或 compact 后自动产生；`PI_MEMORY_AUTO_CONSOLIDATE=0` 可关闭。）
9. **隐私边界？**（默认：DB 0600、目录 0700；日志不记录记忆正文/prompt；多用户机器在 README 说明清理与隔离方式。）
10. **模型 hash 的信任来源？**（默认：仓库内版本化 manifest + 固定 upstream revision；manifest 更新走 PR review，像 lockfile 一样对待；自定义模型需自带 manifest。`VERIFY=0` 仅开发/本地构建可用，release 下强制校验。）

---

## 12. 建议的第一批 issue

如果要把计划直接拆成可执行任务，建议先开这 8 个（对应 v0.1.17 止血）：

1. `fix(core): stable content-addressed IDs for LLM-extracted memories`（PR-1.1）
2. `fix(core): transactional retain with completion marker`（PR-1.2）
3. `fix(core): consolidation claim lease with out-of-transaction summary`（PR-1.3，含 stable id / `summary_of` / lease 恢复）
4. `feat(core): episodic embeddings + missing-embedding backfill`（PR-1.4，含模型校验/tokenization）
5. `fix(core): real timestamps for structured memories`（PR-1.5）
6. `feat(core): search_text + CJK terms + FTS top-k`（PR-1.7 / 2.1 最小版，先行）
7. `feat(core): independent vector top-k outside FTS allowlist`（PR-1.6 / 2.2a，依赖 #6；Done 要求 PR-1.7 已合入并在新 FTS 路径通过测试）
8. `fix(core): scope-safe clear and migration`（PR-2.6，可与 1.x 并行但必须随 v0.2.0 发布）

后续 M2/M3 的 issue（完整 RRF、structured terms、outbox、超时、自动 consolidation）在 v0.1.17 发布后再拆。

每个 issue 的 Done 标准：对应测试从红变绿 + `cargo clippy --workspace --all-targets -- -D warnings` + 不影响现有 `bun test`。PR-1.7 与 PR-1.6 必须同批发布，不能只完成其中一个。问题背景和证据直接链接 `docs/REVIEW.md` 的对应小节。

---

## 附录 A：ID 格式

```
{turn_memory_id}:{kind}:{hash16}

例：
session-1:turn:42:fact:9f2c1a7b3d4e5f60
session-1:turn:42:preference:1a2b3c4d5e6f7081
session-1:turn:42:instruction:...
session-1:turn:42:timeline:...
```

hash 输入：

- fact：`normalize(subject) | normalize(predicate) | normalize(object)`
- preference：`normalize(preference) | normalize(topic)`
- instruction：`normalize(instruction) | normalize(topic)`
- timeline：`normalize(date) | normalize(description)`

## 附录 B：RRF 公式

```
候选先按 project_scope 过滤（同一 DB 内多 scope 的 legacy 数据也先过滤）
rank 从 1 开始
score(c) = w_fts  * 1 / (rrf_k + rank_fts(c))
         + w_vec  * 1 / (rrf_k + rank_vec(c))
         + w_str  * 1 / (rrf_k + rank_struct(c))

默认 rrf_k = 60
w_fts = 1.0, w_vec = 1.0, w_str = 0.8

tie-break（仅在 score 完全相同时，不参与分数）：
  1. in_scope（当前 project 优先）
  2. memory_id（升序，保证确定性）
```

## 附录 C：关键 SQL 草图

```sql
-- search_text 列（可空：NULL = 未回填，'' = 原文为空）
ALTER TABLE working_memory ADD COLUMN search_text TEXT;
ALTER TABLE episodic_memory ADD COLUMN search_text TEXT;

-- 迁移期：v1 保留，新建 v2
CREATE INDEX IF NOT EXISTS fts_working_idx_v2 ON working_memory USING fts (search_text);
CREATE INDEX IF NOT EXISTS fts_episodes_idx_v2 ON episodic_memory USING fts (search_text);

-- 切换状态（单独小事务）
UPDATE app_meta SET value='search_text' WHERE key='fts_state';
UPDATE app_meta SET value='2' WHERE key='fts_version';

-- FTS Top-K（不带 scope 谓词，依赖 DB 物理隔离）
SELECT id, content, ..., fts_score(search_text, ?1) AS score
FROM working_memory
WHERE fts_match(search_text, ?1)
ORDER BY score DESC
LIMIT ?2;
```

```sql
-- structured 词项召回（动态拼接 term 数）
SELECT fact_id, subject, predicate, object, confidence, ...
FROM facts
WHERE (
  subject LIKE ?1 OR predicate LIKE ?1 OR object LIKE ?1 OR
  subject LIKE ?2 OR predicate LIKE ?2 OR object LIKE ?2
)
AND (project_scope = ?n OR project_scope = '' OR project_scope IS NULL)
ORDER BY confidence DESC, timestamp DESC
LIMIT ?limit;

-- instructions 另加：AND active = 1
-- structured 四表目前没有 valid_until / superseded_by；过期过滤只用于 working/episodic
```

## 附录 D：建议的配置项

| 环境变量 | 默认 | 说明 |
|---|---|---|
| `PI_MEMORY_FTS_TOP_K` | 50 | FTS 候选数 |
| `PI_MEMORY_VECTOR_TOP_K` | 50 | 向量候选数 |
| `PI_MEMORY_STRUCTURED_TOP_K` | 30 | 结构化候选数 |
| `PI_MEMORY_STRUCTURED_TERM_LIMIT` | 2 | 每次结构化 LIKE 查询的 term 数上限；可设为 1–3 |
| `PI_MEMORY_RRF_K` | 60 | RRF 平滑 |
| `PI_MEMORY_RECALL_MIN_SCORE` | 0.012 | 低于该 RRF 分数的候选丢弃 |
| `PI_MEMORY_RECALL_RELATIVE_CUTOFF` | 0.2 | 低于最高分该比例的候选丢弃 |
| `PI_MEMORY_RECALL_MODE` | `hybrid` | `legacy` = v0.1.16 行为（向量 allowlist + 百分位 FTS）；M1 回退 PR-1.6，M2 回退 RRF |
| `PI_MEMORY_LLM_ASYNC` | 1 | 0 = 同步 retain（调试） |
| `PI_MEMORY_AUTO_CONSOLIDATE` | 1 | 0 = 只手动 consolidation |
| `PI_MEMORY_CONSOLIDATE_EVERY` | 20 | 未 consolidate working 阈值 |
| `PI_MEMORY_CONSOLIDATION_LEASE_MS` | 180000 | consolidation claim lease；心跳间隔 TTL/3 |
| `PI_MEMORY_LEGACY_CONSOLIDATION` | 0 | 1 = 兼容旧 `consolidateSessionMemories(llm_result)`，不保证 `summary_of` |
| `PI_MEMORY_FTS_MIGRATION_LEASE_MS` | 30000 | FTS 迁移 leader lease；心跳间隔 10s |
| `PI_MEMORY_FTS_MODE` | 跟随数据库迁移状态（迁移完成后为 `search_text`） | `content` 强制回退旧索引；`search_text` 跟随持久化状态，不越过未完成迁移；`ngram` 和未知值明确报错 |
| `PI_MEMORY_MAX_OPEN_PROJECTS` | 16 | runtime LRU；范围 1–256，仅驱逐无共享引用的 idle 项目 |
| `PI_MEMORY_MODEL_MANIFEST` | 空 | 自定义模型 manifest 路径 |
| `PI_MEMORY_MODEL_VERIFY` | 1 | 0 = 跳过模型 hash 校验；**仅 dev/debug 构建生效**，仍校验结构/维度/tokenizer |
| `PI_MEMORY_EMBEDDING_MODEL` | `all-MiniLM-L6-v2` | 活动模型 ID，必须匹配 manifest 的 `model_id`（PR-2.5） |

---

## 13. 结论

原报告的方向是对的：**先修检索覆盖、数据一致性、多会话可靠性和 LLM 成本，不要换数据库/向量引擎。** 本计划在其基础上做了四件事：

1. 把“数据丢失”拆成可独立发布的 PR，先止血；
2. 把“双路召回”拆成“查询形状/中文词项”和“独立 Top-K/RRF”两步；其中前两步（PR-1.7 / PR-1.6）前移到 v0.1.17，避免止血版修了写入却搜不到；
3. 把 `clear`/迁移 scope、时间戳、模型完整性、tokenization 这些“会直接造成数据/隐私问题”的项从 M4 提前，不留在文档里；
4. 把 RPC 超时、`turn_end` 阻塞、socket 生命周期、outbox 和自动 consolidation 放到 M3，和召回质量解耦。

如果只能做三件事，按顺序做：**PR-1.1（多条目 ID）、PR-1.2（retain 事务）、PR-1.3（consolidation 事务）**；紧接着做 **PR-1.4（episodic 向量）、PR-1.7（中文）、PR-1.6（独立向量）**。前三项保证“写入不丢、重试不重复”，后三项保证“长期记忆真的能被搜到”；之后再谈 RRF 和排序质量。

按当前范围，单人顺序执行约 **8–10 周**；M1（v0.1.17）约 **2.5–3 周**。如果 S1 不通过，PR-1.7 与依赖它的 PR-1.6 都不得进入 v0.1.17；先实现并验证替代 FTS 查询方案，只有替代方案通过 CJK、Top-K 和性能门槛后，才允许两项同批发布。若替代方案也未通过，则该版本明确标注中文 FTS 未修复，并将中文召回列为已知 P0；不得只发布 PR-1.6 或宣称完成止血。

第三轮审阅指出的 partial claim、`source_sha` 完成态、PR-1.7 → PR-1.6 顺序和模型 manifest 已并入（见 1.5）。第四轮审阅指出的 consolidation 事务边界、FTS 多实例迁移、clear/migrate 原子性承诺和 `VERIFY=0` 边界已并入（见 1.6）。第五轮补充的 Retain 快照冲突、FTS fencing、clear dirty 状态持久化和 S1 发布门禁已纳入测试矩阵 #59–#61。第六轮明确旧 server 回滚写入可能产生的 NULL `search_text`：新版恢复时先退回 v1、查询可用，再回填并切回 v2；见 8.1/8.3 和测试 #62。第七轮补齐 stale LLM 结果不复用、已 consolidated working 更新后的旧 episodic 失效/重算、migrate 崩溃 dirty 状态和 FTS DDL fencing 边界（见 1.7 / PR-1.2 / PR-2.6 / 8.1，测试 #63–#67）；随后新增默认 30 秒生产租约的跨进程 heartbeat 覆盖（测试 #68），四表 10k structured gate 与英文 FTS stop-word 回归（测试 #69–#70），PR-4.1 recall 注入预算逻辑的单元覆盖（测试 #71），purge scope/embedding/向量 hydrate 一致性覆盖（测试 #72），diagnose 成本指标生命周期覆盖（测试 #73），Unix 数据目录与主数据库文件权限覆盖（测试 #74），有引用保护的 idle runtime LRU 验收（测试 #75），以及 legacy partial retain 重放和 LLM-only 旧项清理（测试 #76）。

当前计划的可执行边界是：**事务只覆盖 DB 写入，长操作（LLM、embedding、FTS/向量重建）用 lease + 版本校验 + 可重入状态机放在事务外；任何跨事务步骤都不承诺整体回滚，只承诺可恢复的 `partial` 状态。** 按这个边界拆 issue，验收标准可以直接对应测试矩阵里的 #47–#76。
