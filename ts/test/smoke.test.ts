import { test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";

test("resident process lifecycle and JSON-RPC pipeline", async () => {
  const logs: string[] = [];
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    socketMode: false, // 生命周期断言针对 stdio 语义（exit code / kill）
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
    socketMode: false, // 生命周期断言针对 stdio 语义（exit code / kill）
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

test("onData 畸形帧不抛异常且超大帧触发重置", async () => {
  const logs: string[] = [];
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    socketMode: false, // 生命周期断言针对 stdio 语义（exit code / kill）
    log: (line) => logs.push(line),
  });
  await client.ensureStarted();
  const anyClient = client as unknown as { onData: (chunk: Buffer) => void; proc: unknown };

  // 1) 垃圾字节（非帧结构）→ 不抛异常
  expect(() => anyClient.onData(Buffer.from([0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03]))).not.toThrow();
  // 2) 随机 fuzz：100 组随机 Buffer → 不抛异常
  const rand = (n: number) => Buffer.from(Array.from({ length: n }, () => Math.floor(Math.random() * 256)));
  for (let i = 0; i < 100; i++) {
    expect(() => anyClient.onData(rand(i % 32))).not.toThrow();
  }
  // 3) 超大帧（>16MB）→ 触发进程重置（kill），不挂起
  const clientWithProc = client as unknown as { proc: { pid: number } | null };
  const originalPid = clientWithProc.proc?.pid;
  const oversized = Buffer.alloc(4);
  oversized.writeUInt32BE(17 * 1024 * 1024, 0); // 17MB > MAX_FRAME_BYTES
  expect(() => anyClient.onData(oversized)).not.toThrow();
  await new Promise((resolve) => setTimeout(resolve, 300));
  expect(logs.some((line) => line.includes("oversized frame"))).toBe(true);
  // 进程被 kill 后自动重启（非 shuttingDown），proc 应被替换为新的 pid
  expect(clientWithProc.proc?.pid).not.toBe(originalPid);
  expect(clientWithProc.proc?.pid).toBeGreaterThan(0);

  await client.shutdown().catch(() => {});
});

test("socket 单例：两个 client 复用同一 server 进程", async () => {
  const logsA: string[] = [];
  const logsB: string[] = [];
  const a = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    socketMode: true,
    log: (line) => logsA.push(line),
  });
  const b = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    socketMode: true,
    log: (line) => logsB.push(line),
  });
  // A 先启动（spawn server）
  await a.ensureStarted();
  const procA = (a as unknown as { proc: { pid: number } | null }).proc;
  expect(procA).not.toBe(null);
  // B 连接（复用同一 server，不 spawn 新进程）
  await b.ensureStarted();
  const procB = (b as unknown as { proc: { pid: number } | null }).proc;
  expect(procB).toBe(null); // B 未 spawn：proc 为 null（复用了 A 的 server）
  // 双方都能工作
  const scope = `/tmp/socket-shared-${Date.now()}`;
  await a.call("retainTurnMemories", { turnIndex: 1, text: "shared alpha", sessionId: "sa", projectScope: scope, llmResult: null, llmFailed: false });
  await b.call("retainTurnMemories", { turnIndex: 1, text: "shared bravo", sessionId: "sb", projectScope: scope, llmResult: null, llmFailed: false });
  const recall = await b.call<string>("buildRecallInjection", { prompt: "shared", sessionId: "sb", projectScope: scope });
  expect(recall.includes("shared bravo") && recall.includes("shared alpha")).toBe(true);
  // A 退出后 server 仍存活（B 继续）
  await a.shutdown();
  await new Promise((resolve) => setTimeout(resolve, 400));
  const stats = await b.call<string>("memoryCommand", { sub: "stats", target: "", sessionId: "sb", projectScope: scope });
  expect(stats).toContain("working=2");
  // B 退出 → server 空闲退出（等 server 自行清理，给足时间）
  await b.shutdown();
  await new Promise((resolve) => setTimeout(resolve, 400));
});
