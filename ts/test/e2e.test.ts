import { test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";
import * as os from "node:os";
import * as path from "node:path";

test("end-to-end: retain → recall → consolidate → commands", async () => {
  // Unique scope: data persists per projectScope, so repeated runs are not idempotently skipped
  const scope = path.join(os.tmpdir(), `pi-memory-e2e-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    callTimeoutMs: 5000,
  });

  // retain one turn (with structured extraction)
  await client.call("retainTurnMemories", {
    turnIndex: 1,
    text: "事实: 项目正式采用 Rust 重写\n偏好: 回复使用中文",
    sessionId: "s1",
    projectScope: scope,
  });

  // recall: FTS hits working + structured fact
  const recall = await client.call<string>("buildRecallInjection", {
    prompt: "Rust 重写", sessionId: "s1", projectScope: scope,
  });
  expect(recall).toContain("- [working]");
  expect(recall).toContain("Rust 重写");

  // structured candidates: the "中文" preference
  const recallZh = await client.call<string>("buildRecallInjection", {
    prompt: "中文", sessionId: "s1", projectScope: scope,
  });
  expect(recallZh).toContain("回复使用中文");

  // consolidate
  const consolidated = await client.call<Record<string, unknown>>("consolidateSessionMemories", {
    sessionId: "s1", projectScope: scope,
  });
  expect(consolidated.episodic_count).toBe(1);

  // commands
  const stats = await client.call<string>("memoryCommand", {
    sub: "stats", target: "", sessionId: "s1", projectScope: scope,
  });
  expect(stats).toContain("working=1");
  expect(stats).toContain("facts=1");

  const diag = await client.call<string>("memoryCommand", {
    sub: "diagnose", target: "", sessionId: "s1", projectScope: scope,
  });
  expect(diag).toMatch(/embedding=(on|off|loading|model-error)/);

  await client.shutdown();
});

