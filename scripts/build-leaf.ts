#!/usr/bin/env bun
/**
 * leaf 包生成：按平台产出 @jackice/pi-memory-rust-<tag> 包。
 *
 * 用法：bun scripts/build-leaf.ts [--tag darwin-arm64]
 * 产物：leaf-packages/<tag>/（含二进制 + package.json）
 * 父包 ts/package.json 通过 optionalDependencies 按平台安装。
 */

import * as fs from "node:fs";
import * as path from "node:path";

const ROOT = path.join(import.meta.dir, "..");
const VERSION = "0.1.1";
const BINARY_NAME = "pi-memory-server";

const TARGETS = [
  { tag: "darwin-arm64", os: "darwin", cpu: "arm64" },
  { tag: "darwin-x64", os: "darwin", cpu: "x64" },
  { tag: "linux-x64", os: "linux", cpu: "x64" },
  { tag: "win32-x64", os: "win32", cpu: "x64" },
];

const tagArg = process.argv.find((a) => a.startsWith("--tag="))?.split("=")[1];
const targets = tagArg ? TARGETS.filter((t) => t.tag === tagArg) : TARGETS;

for (const target of targets) {
  // 二进制来源：target/release（cargo build --release）或交叉编译产物
  const binarySource = path.join(ROOT, "target", "release", BINARY_NAME);
  if (!fs.existsSync(binarySource)) {
    console.warn(`[skip] ${target.tag}: 缺少 ${binarySource}（先 cargo build --release）`);
    continue;
  }
  const leafDir = path.join(ROOT, "leaf-packages", target.tag);
  fs.mkdirSync(leafDir, { recursive: true });
  const binaryTarget = path.join(leafDir, BINARY_NAME + (target.os === "win32" ? ".exe" : ""));
  fs.copyFileSync(binarySource, binaryTarget);
  fs.chmodSync(binaryTarget, 0o755);

  const manifest = {
    name: `@jackice/pi-memory-rust-${target.tag}`,
    version: VERSION,
    description: `pi-memory-rust native binary for ${target.os}-${target.cpu}`,
    license: "MIT",
    os: [target.os],
    cpu: [target.cpu],
    files: [path.basename(binaryTarget)],
    engines: { node: ">=22.19.0" },
  };
  fs.writeFileSync(path.join(leafDir, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
  console.log(`[ok] ${target.tag} → ${leafDir}`);
}

// 汇总：更新 ts/package.json 的 optionalDependencies
const tsPkgPath = path.join(ROOT, "ts", "package.json");
const tsPkg = JSON.parse(fs.readFileSync(tsPkgPath, "utf8"));
tsPkg.optionalDependencies = Object.fromEntries(
  TARGETS.map((t) => [`@jackice/pi-memory-rust-${t.tag}`, `=${VERSION}`]),
);
fs.writeFileSync(tsPkgPath, JSON.stringify(tsPkg, null, 2) + "\n");
console.log("[ok] ts/package.json optionalDependencies 已更新");
