import { test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";
import * as os from "node:os";
import * as path from "node:path";

test("端到端：retain → recall → consolidate → 命令", async () => {
  // 唯一 scope：数据按 projectScope 持久化，避免多次运行幂等跳过
  const scope = path.join(os.tmpdir(), `pi-memory-e2e-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    callTimeoutMs: 5000,
  });

  // retain 一轮（含结构化提取）
  await client.call("retainTurnMemories", {
    turnIndex: 1,
    text: "事实: 项目正式采用 Rust 重写\n偏好: 回复使用中文",
    sessionId: "s1",
    projectScope: scope,
  });

  // recall：FTS 命中 working + 结构化 fact
  const recall = await client.call<string>("buildRecallInjection", {
    prompt: "Rust 重写", sessionId: "s1", projectScope: scope,
  });
  expect(recall).toContain("- [working]");
  expect(recall).toContain("Rust 重写");

  // 结构化候选：偏好「中文」
  const recallZh = await client.call<string>("buildRecallInjection", {
    prompt: "中文", sessionId: "s1", projectScope: scope,
  });
  expect(recallZh).toContain("回复使用中文");

  // consolidate
  const consolidated = await client.call<Record<string, unknown>>("consolidateSessionMemories", {
    sessionId: "s1", projectScope: scope,
  });
  expect(consolidated.episodic_count).toBe(1);

  // 命令
  const stats = await client.call<string>("memoryCommand", {
    sub: "stats", target: "", sessionId: "s1", projectScope: scope,
  });
  expect(stats).toContain("working=1");
  expect(stats).toContain("facts=1");

  const diag = await client.call<string>("memoryCommand", {
    sub: "diagnose", target: "", sessionId: "s1", projectScope: scope,
  });
  expect(diag).toMatch(/embedding=(on|off|model-error)/);

  await client.shutdown();
});
