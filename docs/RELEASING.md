# 发布流程（RELEASING）

`@jackice/pi-memory-rust` 采用 **父包 + leaf 包** 分发（参照 oh-my-pi）：

- **父包** `@jackice/pi-memory-rust`：TS 薄壳（index.ts + src/client.ts），纯 JS 无二进制，~15KB
- **leaf 包** `@jackice/pi-memory-rust-<platform>-<arch>`：每平台一个 Rust 二进制（pi-memory-server）
- 父包 `optionalDependencies` 引用所有 leaf 包，client 运行时按 `process.platform-arch` 解析对应 leaf 包

`0.1.17` 仅发布 macOS arm64。父包用 `os`/`cpu` 限定该平台，直到其他架构的真实二进制完成构建和发布。

## 版本

父包版本以 `ts/package.json` 为准；`scripts/build-leaf.ts` 从父包读取版本，并只更新本次实际生成的 leaf optional dependency。每个 `leaf-packages/<platform>/package.json` 必须与父包版本一致后才能发布该平台。

## 发布步骤

## 数据库迁移与回滚

- 新版本只增加 schema 字段和表；发布前保留每个项目的 `memory.db` 备份，并在副本上演练升级与回滚。
- v0.2 保留 `content` FTS 索引供旧版读取。`search_text` 回填或新索引探测失败时，查询继续使用旧索引；确认新版已完成 FTS 和向量恢复后再清除 dirty 状态。
- `/memory migrate <path>` 只导入当前项目 scope。数据库导入事务提交后，FTS/向量重建失败会报告 `db_committed=true`、`index_state=partial`；此时不要重跑整库清理，先运行 `/memory rebuild` 和 `/memory rebuild-embeddings` 收敛索引。
- 旧版本的 `/memory clear <target>` 曾可能在调用方当前项目数据库中删除匹配 `<target>` scope 的记录，而不是打开并清理目标项目数据库。若在受影响版本中执行过该命令，目标项目或调用方项目的数据可能已被删除；该操作无法从当前数据库自动恢复，只能从执行前备份恢复，或重新 retain 原始会话内容。升级不会补回已删除的数据。
- `prepare-model`、`migrate`、`rebuild-embeddings` 通过 `startJob`/`jobStatus` 执行；30 分钟内未结束时记录 job id，并用 `/memory job <id>` 继续查看。job 状态保存在 server 内存中，server 重启后需重新检查数据库的 dirty 状态并运行对应恢复命令。
- 回滚代码前停止旧、新 server 对同一数据库的并发写入。新版重新启动时必须检查 `search_text` NULL 行并先回退到可用的 `content` 索引，再执行回填和验证。
- 发布说明需披露旧版已覆盖的多条 structured facts 无法从数据库恢复；有需要时用户可重新 retain 原始 turn。

### 1. 构建本机二进制

```bash
cd /Users/jackice/dev/src/github.com/jackice/pi-memory-rust
cargo build --release
cp target/release/pi-memory-server ts/bin/
```

### 2. 生成 leaf 包

```bash
bun scripts/build-leaf.ts --tag=darwin-arm64
# 仅可在对应宿主架构构建；脚本拒绝将本机二进制标记为其他平台。
# 产物：leaf-packages/darwin-arm64/（二进制 + package.json）
# 只更新 ts/package.json 中本次实际构建的平台依赖
```

> 其他平台（linux-x64 / darwin-x64 / win32-x64）需要在对应平台构建并运行脚本，再发布相应 leaf 包。不要把本机二进制复制或标记成其他架构。

### 3. 验证打包内容

```bash
cd ts && npm pack --dry-run
# 应只含 README.md / index.ts / package.json / src/client.ts（无二进制）
```

### 4. 发布顺序（先 leaf 后父包）

```bash
# 当前宿主平台 leaf 包（0.1.17 当前仅 darwin-arm64）
cd leaf-packages/darwin-arm64 && npm publish --access public

# 父包
cd ts && npm publish --access public
```

### 5. 安装验证

```bash
pi install npm:@jackice/pi-memory-rust
# 或本地验证
pi install /Users/jackice/dev/src/github.com/jackice/pi-memory-rust/ts
```

## 常见问题

- **leaf 包未发布/缺失**：client 回退到包内 `bin/`（本地开发）或报诊断错误（提示 `PI_MEMORY_RUST_BINARY`）
- **模型分发**：embedding 模型（~90MB）不在 npm 包内，首次使用自动下载（HF 官方/镜像），或设置 `PI_MEMORY_MODEL_PATH`
- **模型切换**：自定义模型需 `PI_MEMORY_EMBEDDING_MODEL`、`PI_MEMORY_MODEL_PATH` 和绝对路径的 pinned `PI_MEMORY_MODEL_MANIFEST`。启动时会把旧的单模型 embedding 主键迁移为 `(memory_kind, memory_id, model)`，切换后运行 `/memory rebuild-embeddings`；旧模型向量保留，可切回并复用。模型升级前先备份项目 DB。
- **平台匹配**：leaf 包声明 `os`/`cpu`，npm 按平台安装；不匹配的 leaf 包不会被安装
