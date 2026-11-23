#!/usr/bin/env bun
/**
 * leaf 包生成：按平台产出 @jackice/pi-memory-rust-<tag> 包。
 *
 * 用法：bun scripts/build-leaf.ts [--tag=darwin-arm64]
 * 产物：leaf-packages/<tag>/（含二进制 + package.json）
 * 仅打包当前宿主架构，避免把本机二进制误标成其他平台。
 * 父包只更新本次实际生成的 leaf optionalDependency。
 */

import * as fs from "node:fs";
import * as path from "node:path";

const ROOT = path.join(import.meta.dir, "..");
const BINARY_NAME = "pi-memory-server";

const TARGETS = [
  { tag: "darwin-arm64", os: "darwin", cpu: "arm64" },
  { tag: "darwin-x64", os: "darwin", cpu: "x64" },
  { tag: "linux-x64", os: "linux", cpu: "x64" },
  { tag: "win32-x64", os: "win32", cpu: "x64" },
];

const tsPkgPath = path.join(ROOT, "ts", "package.json");
const tsPkg = JSON.parse(fs.readFileSync(tsPkgPath, "utf8"));
const VERSION = tsPkg.version;
const tagIndex = process.argv.indexOf("--tag");
const tagArg = process.argv.find((a) => a.startsWith("--tag="))?.split("=")[1]
  ?? (tagIndex >= 0 ? process.argv[tagIndex + 1] : undefined);
const hostTag = `${process.platform}-${process.arch}`;
const targetTag = tagArg ?? hostTag;
const targets = TARGETS.filter((target) => target.tag === targetTag);

if (targets.length !== 1) {
  throw new Error(`unsupported target ${targetTag}; choose one of ${TARGETS.map((t) => t.tag).join(", ")}`);
}
if (targetTag !== hostTag) {
  throw new Error(`refusing to package ${targetTag} from ${hostTag}; cross-compile a matching binary first`);
}

const builtTargets: typeof TARGETS = [];
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
  builtTargets.push(target);
  console.log(`[ok] ${target.tag} → ${leafDir}`);
}

// 汇总：更新 ts/package.json 的 optionalDependencies
if (builtTargets.length === 0) {
  throw new Error(`no leaf binary was built for ${targetTag}`);
}
tsPkg.optionalDependencies ??= {};
for (const target of builtTargets) {
  tsPkg.optionalDependencies[`@jackice/pi-memory-rust-${target.tag}`] = `=${VERSION}`;
}
fs.writeFileSync(tsPkgPath, JSON.stringify(tsPkg, null, 2) + "\n");
console.log("[ok] ts/package.json optionalDependencies 已更新");
