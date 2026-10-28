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
      // 模拟 Rust 侧 RecallParams 字段校验：prompt 必须是字符串
      if (typeof (params as { prompt?: unknown } | undefined)?.prompt !== "string") {
        throw new Error("invalid buildRecallInjection params");
      }
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

test("session_shutdown asks on quit and summarizes after user confirmation", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, true);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(1);
  expect(confirmCalls[0][0]).toContain("Summarize");
  expect(confirmCalls[0][2]?.timeout).toBeDefined();
  expect(callLog.map(([method]) => method)).toEqual([
    "llmPrompts",
    "consolidateSource",
    "consolidateSessionMemories",
  ]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown defaults to no summary on quit", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, false);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(1);
  expect(callLog.map(([method]) => method)).toEqual([]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown neither asks nor summarizes on non-quit", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, true);

  await emitSessionShutdown(pi, { reason: "reload" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual([]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown neither asks nor summarizes without UI", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(false, true);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual([]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown still exits and closes the client when the LLM times out on quit", async () => {
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

test("turn_end LLM extraction is not killed by the 1s timeout (slow model still passes llmResult)", async () => {
  const prevEnv = process.env.PI_MEMORY_LLM;
  process.env.PI_MEMORY_LLM = "on";
  try {
    mockClientMethods();
    const pi = new FakePi();
    registerPiMemoryRustExtension(pi as any, {});
    const ctx = makeCtx(false, true).ctx;
    (ctx as any).model = { id: "m", provider: "p" };
    (ctx as any).modelRegistry = {
      complete: async () => {
        // Slower than the old 1s shutdown timeout, but within the generous turn timeout
        await new Promise((resolve) => setTimeout(resolve, 1500));
        return { content: [{ type: "text", text: '{"facts":[{"subject":"s","predicate":"p","object":"o"}]}' }] };
      },
    };

    const handler = pi.handlers.get("turn_end")?.[0];
    expect(handler).toBeDefined();
    await handler!({ message: { content: "test text" }, turnIndex: 1 }, ctx);

    const retain = callLog.find(([method]) => method === "retainTurnMemories");
    expect(retain).toBeDefined();
    expect((retain![1] as { llmResult?: unknown }).llmResult).toContain("facts");
    expect((retain![1] as { llmFailed?: unknown }).llmFailed).toBe(false);
  } finally {
    if (prevEnv === undefined) {
      delete process.env.PI_MEMORY_LLM;
    } else {
      process.env.PI_MEMORY_LLM = prevEnv;
    }
  }
});

test("before_agent_start skips recall for custom-message runs without a prompt", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  const handler = pi.handlers.get("before_agent_start")?.[0];
  expect(handler).toBeDefined();
  const result = await handler!({ messageType: "custom", message: { content: "welcome" } }, ctx);

  expect(result).toBeUndefined();
  expect(callLog.map(([method]) => method)).toEqual([]);
});

test("before_agent_start recalls with the user prompt", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  const handler = pi.handlers.get("before_agent_start")?.[0];
  expect(handler).toBeDefined();
  const result = (await handler!({ messageType: "user", prompt: "hello memory" }, ctx)) as any;

  expect(callLog.map(([method]) => method)).toEqual(["buildRecallInjection"]);
  expect((callLog[0][1] as { prompt?: unknown }).prompt).toBe("hello memory");
  expect(result.message.content).toBe("injection");
});

