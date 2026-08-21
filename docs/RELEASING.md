# 发布流程（RELEASING）

`@jackice/pi-memory-rust` 采用 **父包 + leaf 包** 分发（参照 oh-my-pi）：

- **父包** `@jackice/pi-memory-rust`：TS 薄壳（index.ts + src/client.ts），纯 JS 无二进制，~15KB
- **leaf 包** `@jackice/pi-memory-rust-<platform>-<arch>`：每平台一个 Rust 二进制（pi-memory-server）
- 父包 `optionalDependencies` 引用所有 leaf 包，client 运行时按 `process.platform-arch` 解析对应 leaf 包

## 版本

版本号三处同步：`ts/package.json`（父包）、`leaf-packages/*/package.json`（leaf 包）、`scripts/build-leaf.ts` 的 `VERSION` 常量。

## 发布步骤

### 1. 构建本机二进制

```bash
cd /Users/jackice/dev/src/github.com/jackice/pi-memory-rust
cargo build --release
cp target/release/pi-memory-server ts/bin/
```

### 2. 生成 leaf 包

```bash
bun scripts/build-leaf.ts
# 产物：leaf-packages/<tag>/（二进制 + package.json）
# 自动更新 ts/package.json 的 optionalDependencies
```

> 其他平台（linux-x64 / darwin-x64 / win32-x64）需要 CI 或交叉编译后在对应平台运行 build-leaf。当前仅本机 darwin-arm64 可发布。

### 3. 验证打包内容

```bash
cd ts && npm pack --dry-run
# 应只含 README.md / index.ts / package.json / src/client.ts（无二进制）
```

### 4. 发布顺序（先 leaf 后父包）

```bash
# leaf 包们
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
- **平台匹配**：leaf 包声明 `os`/`cpu`，npm 按平台安装；不匹配的 leaf 包不会被安装
