import { afterAll, beforeAll, test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const previousDataDir = process.env.PI_MEMORY_DATA_DIR;
const testTempRoot = process.platform === "win32" ? os.tmpdir() : "/tmp";
const testDataDir = path.join(testTempRoot, `pm-e2e-${process.pid}-${Date.now()}`);

beforeAll(() => {
  process.env.PI_MEMORY_DATA_DIR = testDataDir;
});

afterAll(() => {
  if (previousDataDir === undefined) delete process.env.PI_MEMORY_DATA_DIR;
  else process.env.PI_MEMORY_DATA_DIR = previousDataDir;
  fs.rmSync(testDataDir, { recursive: true, force: true });
});

test("end-to-end: retain → recall → consolidate → commands", async () => {
  // Unique scope: data persists per projectScope, so repeated runs are not idempotently skipped
  const scope = path.join(os.tmpdir(), `pi-memory-e2e-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    socketMode: false,
    callTimeoutMs: 5000,
  });

  try {
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
  const claim = await client.call<{ claimToken: string; working: Array<{ id: string }> }>("claimConsolidation", {
    sessionId: "s1", projectScope: scope, ttlMs: 180_000, limit: 100,
  });
  expect(claim.working).toHaveLength(1);
  const consolidated = await client.call<{ committed: boolean }>("commitConsolidation", {
    projectScope: scope, claimToken: claim.claimToken, summary: "项目采用 Rust 重写；用户偏好中文回复", degraded: true,
  });
  expect(consolidated.committed).toBe(true);

  // commands
  const stats = await client.call<string>("memoryCommand", {
    sub: "stats", target: "", sessionId: "s1", projectScope: scope,
  });
  expect(stats).toContain("working=1");
  expect(stats).toContain("facts=1");
  expect(stats).toContain("episodic=1");

  const diag = await client.call<string>("memoryCommand", {
    sub: "diagnose", target: "", sessionId: "s1", projectScope: scope,
  });
  expect(diag).toMatch(/embedding=(on|off|loading|model-error)/);

  } finally {
    await client.shutdown().catch(() => {});
  }
});

