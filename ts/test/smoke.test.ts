import { test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";

test("resident process lifecycle and JSON-RPC pipeline", async () => {
  const logs: string[] = [];
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    log: (line) => logs.push(line),
  });

  // start + handshake
  await client.ensureStarted();

  // diagnose
  const diag = await client.diagnose();
  expect(diag.backend).toBe("turso");

  // unknown method should throw
  await expect(client.call("nope")).rejects.toThrow();

  // graceful shutdown
  await client.shutdown();
  expect(logs.some((line) => line.includes("SIGKILL"))).toBe(false);
  expect(logs.some((line) => line.includes("exit code 0"))).toBe(true);
});


test("shutdown 后不再自动重启进程（防止孤儿进程泄漏）", async () => {
  const logs: string[] = [];
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    log: (line) => logs.push(line),
  });
  await client.ensureStarted();
  await client.shutdown();
  // shutdown 后再调用应直接抛错，而不是悄悄拉起新进程
  await expect(client.call("diagnose")).rejects.toThrow();
  expect(logs.some((line) => line.includes("auto-restart"))).toBe(false);
  // 进程应保持关闭（不会出现第二个 hello 握手）
  await new Promise((resolve) => setTimeout(resolve, 200));
  expect(logs.filter((line) => line.includes("hello")).length).toBe(0);
});
