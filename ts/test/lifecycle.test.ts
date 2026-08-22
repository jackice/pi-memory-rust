import { afterEach, expect, test } from "bun:test";
import registerPiMemoryRustExtension from "../index.ts";
import { MemoryRustClient } from "../src/client.ts";

type Handler = (event: any, ctx: any) => Promise<unknown> | unknown;

class FakePi {
  handlers = new Map<string, Handler[]>();
  commands = new Map<string, unknown>();

  on(event: string, handler: Handler): void {
    const list = this.handlers.get(event) ?? [];
    list.push(handler);
    this.handlers.set(event, list);
  }

  registerCommand(name: string, options: unknown): void {
    this.commands.set(name, options);
  }
}

const originalCall = MemoryRustClient.prototype.call;
const originalEnsureStarted = MemoryRustClient.prototype.ensureStarted;
const originalShutdown = MemoryRustClient.prototype.shutdown;

let callLog: Array<[string, unknown]> = [];
let shutdownCount = 0;

function mockClientMethods() {
  callLog = [];
  shutdownCount = 0;
  MemoryRustClient.prototype.ensureStarted = async () => {};
  MemoryRustClient.prototype.call = async function (method: string, params: unknown = {}, timeoutMs?: number) {
    callLog.push([method, params]);
    if (method === "llmPrompts") {
      return { extract: "EXTRACT", consolidate: "CONSOLIDATE" };
    }
    if (method === "consolidateSource") {
      return "source text";
    }
    if (method === "consolidateSessionMemories") {
      return { ok: true };
    }
    if (method === "memoryCommand") {
      return "ok";
    }
    if (method === "buildRecallInjection") {
      return "injection";
    }
    if (method === "retainTurnMemories") {
      return undefined;
    }
    if (method === "hello") {
      return undefined;
    }
    if (timeoutMs) {
      return new Promise(() => {});
    }
    return undefined;
  } as typeof MemoryRustClient.prototype.call;
  MemoryRustClient.prototype.shutdown = async () => {
    shutdownCount += 1;
  };
}

afterEach(() => {
  MemoryRustClient.prototype.call = originalCall;
  MemoryRustClient.prototype.ensureStarted = originalEnsureStarted;
  MemoryRustClient.prototype.shutdown = originalShutdown;
});

function makeCtx(hasUI = true, confirmResult = true) {
  const confirmCalls: Array<[string, string, any?]> = [];
  const ctx = {
    hasUI,
    cwd: "/tmp/pi-memory-test",
    sessionManager: { getSessionId: () => "session-1" },
    ui: hasUI
      ? {
          confirm: async (title: string, message: string, opts?: any) => {
            confirmCalls.push([title, message, opts]);
            return confirmResult;
          },
          notify: () => {},
        }
      : undefined,
  };
  return { ctx, confirmCalls };
}

async function emitSessionShutdown(pi: FakePi, event: any, ctx: any) {
  const handler = pi.handlers.get("session_shutdown")?.[0];
  expect(handler).toBeDefined();
  await handler!(event, ctx);
}

test("session_shutdown 在 quit 时询问，用户确认后才总结", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, true);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(1);
  expect(confirmCalls[0][0]).toContain("总结");
  expect(confirmCalls[0][2]?.timeout).toBeDefined();
  expect(callLog.map(([method]) => method)).toEqual([
    "llmPrompts",
    "consolidateSource",
    "consolidateSessionMemories",
  ]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown 在 quit 时默认不总结", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, false);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(1);
  expect(callLog.map(([method]) => method)).toEqual([]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown 在非 quit 场景不询问也不总结", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, true);

  await emitSessionShutdown(pi, { reason: "reload" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual([]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown 在无 UI 场景不询问也不总结", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(false, true);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual([]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown 在 quit 时 LLM 超时也会结束并关闭客户端", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(true, true);
  (ctx as any).model = { id: "m", provider: "p" };
  (ctx as any).modelRegistry = {
    complete: async () => new Promise(() => {}),
  };

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(callLog.map(([method]) => method)).toEqual([
    "llmPrompts",
    "consolidateSource",
    "consolidateSessionMemories",
  ]);
  expect(shutdownCount).toBe(1);
});
