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
const originalAutoConsolidate = process.env.PI_MEMORY_AUTO_CONSOLIDATE;
const originalConsolidateEvery = process.env.PI_MEMORY_CONSOLIDATE_EVERY;
const originalPiSessionId = process.env.PI_SESSION_ID;

let callLog: Array<[string, unknown]> = [];
let shutdownCount = 0;
let nextPendingTurn: Record<string, unknown> | null = null;
let nextConsolidationClaim: Record<string, unknown> | null = null;
let retainFailuresBeforeSuccess = 0;
let unconsolidatedCount = 0;
let enqueueTurnUnsupported = false;
let recallInjection = "injection";
let memoryCommandResult = "ok";

function mockClientMethods() {
  callLog = [];
  shutdownCount = 0;
  nextPendingTurn = null;
  nextConsolidationClaim = null;
  retainFailuresBeforeSuccess = 0;
  unconsolidatedCount = 0;
  enqueueTurnUnsupported = false;
  recallInjection = "injection";
  memoryCommandResult = "ok";
  MemoryRustClient.prototype.ensureStarted = async () => {};
  MemoryRustClient.prototype.call = async function (method: string, params: unknown = {}, timeoutMs?: number) {
    callLog.push([method, params]);
    if (method === "llmPrompts") {
      return { extract: "EXTRACT", consolidate: "CONSOLIDATE" };
    }
    if (method === "claimPendingTurn") {
      const turn = nextPendingTurn;
      nextPendingTurn = null;
      return turn;
    }
    if (method === "claimConsolidation") {
      const claim = nextConsolidationClaim ?? { claimToken: "empty-claim", working: [] };
      nextConsolidationClaim = null;
      return claim;
    }
    if (method === "completePendingTurn") return { completed: true };
    if (method === "countUnconsolidated") return { count: unconsolidatedCount, dirty: false };
    if (method === "commitConsolidation") return { committed: true, snapshotChanged: false };
    if (method === "heartbeatConsolidation" || method === "heartbeatPendingTurn") return { renewed: true };
    if (method === "enqueueTurn") {
      if (enqueueTurnUnsupported) throw new Error("unknown method enqueueTurn");
      return { queued: true };
    }
    if (method === "getWorkingContent") return { text: "latest working text" };
    if (method === "consolidateSource") {
      return "source text";
    }
    if (method === "consolidateSessionMemories") {
      return { ok: true };
    }
    if (method === "memoryCommand") {
      return memoryCommandResult;
    }
    if (method === "buildRecallInjection") {
      // 模拟 Rust 侧 RecallParams 字段校验：prompt 必须是字符串
      if (typeof (params as { prompt?: unknown } | undefined)?.prompt !== "string") {
        throw new Error("invalid buildRecallInjection params");
      }
      return recallInjection;
    }
    if (method === "retainTurnMemories") {
      if (retainFailuresBeforeSuccess > 0) {
        retainFailuresBeforeSuccess -= 1;
        throw new Error("SNAPSHOT_CHANGED: working memory changed during extraction");
      }
      return undefined;
    }
    if (method === "hello") {
      return undefined;
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
  if (originalAutoConsolidate === undefined) delete process.env.PI_MEMORY_AUTO_CONSOLIDATE;
  else process.env.PI_MEMORY_AUTO_CONSOLIDATE = originalAutoConsolidate;
  if (originalConsolidateEvery === undefined) delete process.env.PI_MEMORY_CONSOLIDATE_EVERY;
  else process.env.PI_MEMORY_CONSOLIDATE_EVERY = originalConsolidateEvery;
  if (originalPiSessionId === undefined) delete process.env.PI_SESSION_ID;
  else process.env.PI_SESSION_ID = originalPiSessionId;
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

test("session_shutdown drains pending turns and consolidates without confirmation", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "1";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, true);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual(["claimPendingTurn", "llmPrompts", "claimConsolidation"]);
  expect(shutdownCount).toBe(1);
});

test("session_start replays a durable pending turn after restart", async () => {
  mockClientMethods();
  nextPendingTurn = {
    sessionId: "session-1", turnIndex: 11, projectScope: "/tmp/pi-memory-test",
    text: "survived previous process", promptMissing: true, attempts: 1,
  };
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  await pi.handlers.get("session_start")![0]({}, ctx);
  const deadline = Date.now() + 1_000;
  while (!callLog.some(([method]) => method === "completePendingTurn") && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  expect(callLog.map(([method]) => method)).toContain("claimPendingTurn");
  expect(callLog.map(([method]) => method)).toContain("retainTurnMemories");
  expect(callLog.map(([method]) => method)).toContain("completePendingTurn");
});

test("session_shutdown can disable automatic consolidation", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "0";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, false);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual(["claimPendingTurn"]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown drains turns on non-quit events", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "0";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(true, true);

  await emitSessionShutdown(pi, { reason: "reload" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual(["claimPendingTurn"]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown drains turns without UI", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "0";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx, confirmCalls } = makeCtx(false, true);

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(confirmCalls.length).toBe(0);
  expect(callLog.map(([method]) => method)).toEqual(["claimPendingTurn"]);
  expect(shutdownCount).toBe(1);
});

test("session_shutdown commits a degraded summary when the LLM times out", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "1";
  nextConsolidationClaim = {
    claimToken: "claim-1",
    working: [{ id: "w1", content: "working text", sourceSha: "sha1" }],
  };
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(true, true);
  (ctx as any).model = { id: "m", provider: "p" };
  (ctx as any).modelRegistry = {
    complete: async () => new Promise(() => {}),
  };

  await emitSessionShutdown(pi, { reason: "quit" }, ctx);

  expect(callLog.map(([method]) => method)).toContain("claimConsolidation");
  expect(callLog.map(([method]) => method)).toContain("commitConsolidation");
  const commit = callLog.find(([method]) => method === "commitConsolidation");
  expect((commit![1] as { degraded?: boolean }).degraded).toBe(true);
  expect(shutdownCount).toBe(1);
});

test("turn_end enqueues immediately and background extraction keeps the LLM result", async () => {
  const prevEnv = process.env.PI_MEMORY_LLM;
  process.env.PI_MEMORY_LLM = "on";
  try {
    mockClientMethods();
    nextPendingTurn = {
      sessionId: "session-1", turnIndex: 1, projectScope: "/tmp/pi-memory-test",
      text: "test text", promptMissing: true, attempts: 1,
    };
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
    const startedAt = Date.now();
    await handler!({ message: { content: "test text" }, turnIndex: 1 }, ctx);
    expect(Date.now() - startedAt).toBeLessThan(100);

    const deadline = Date.now() + 3_000;
    while (!callLog.some(([method]) => method === "retainTurnMemories") && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
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

test("outbox retries snapshot conflicts with the latest text and a fresh LLM result", async () => {
  const previousLlm = process.env.PI_MEMORY_LLM;
  process.env.PI_MEMORY_LLM = "on";
  try {
    mockClientMethods();
    nextPendingTurn = {
      sessionId: "session-1", turnIndex: 2, projectScope: "/tmp/pi-memory-test",
      text: "stale pending text", promptMissing: false, attempts: 1,
    };
    retainFailuresBeforeSuccess = 1;
    const pi = new FakePi();
    registerPiMemoryRustExtension(pi as any, {});
    const { ctx } = makeCtx(false);
    const llmInputs: string[] = [];
    (ctx as any).model = { id: "m", provider: "p" };
    (ctx as any).modelRegistry = {
      complete: async (_model: unknown, context: any) => {
        const prompt = context.messages[0].content as string;
        llmInputs.push(prompt);
        const object = prompt.includes("latest working text") ? "fresh result" : "stale result";
        return { content: [{ type: "text", text: JSON.stringify({ facts: [{ subject: "snapshot", predicate: "is", object }] }) }] };
      },
    };

    await pi.handlers.get("turn_end")![0]({ message: { content: "assistant" }, turnIndex: 2 }, ctx);
    const deadline = Date.now() + 3_000;
    while (callLog.filter(([method]) => method === "retainTurnMemories").length < 2 && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }

    const retainCalls = callLog.filter(([method]) => method === "retainTurnMemories");
    expect(retainCalls).toHaveLength(2);
    expect(llmInputs).toHaveLength(2);
    expect(llmInputs[0]).toContain("stale pending text");
    expect(llmInputs[1]).toContain("latest working text");
    expect((retainCalls[0][1] as { llmResult: string }).llmResult).toContain("stale result");
    expect((retainCalls[1][1] as { llmResult: string }).llmResult).toContain("fresh result");
    expect(callLog.map(([method]) => method)).toContain("completePendingTurn");
  } finally {
    if (previousLlm === undefined) delete process.env.PI_MEMORY_LLM;
    else process.env.PI_MEMORY_LLM = previousLlm;
  }
});

test("session_before_compact drains pending turns through the bounded rule path", async () => {
  mockClientMethods();
  nextPendingTurn = {
    sessionId: "session-1", turnIndex: 3, projectScope: "/tmp/pi-memory-test",
    text: "pending before compact", promptMissing: false, attempts: 1,
  };
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  await pi.handlers.get("session_before_compact")![0]({ reason: "token_limit" }, ctx);

  const retain = callLog.find(([method]) => method === "retainTurnMemories");
  expect(retain).toBeDefined();
  expect((retain![1] as { text: string }).text).toBe("pending before compact");
  expect((retain![1] as { llmFailed: boolean }).llmFailed).toBe(true);
  expect(callLog.map(([method]) => method)).toContain("completePendingTurn");
});

test("session_compact persists a checkpoint without blocking on consolidation", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "1";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  await pi.handlers.get("session_compact")![0]({
    reason: "token_limit",
    compactionEntry: { summary: "compact summary", firstKeptEntryId: "entry-7" },
  }, ctx);

  const checkpoint = callLog.find(([method]) => method === "recordCompaction");
  expect(checkpoint).toBeDefined();
  expect(checkpoint![1]).toMatchObject({
    sessionId: "session-1",
    projectScope: "/tmp/pi-memory-test",
    reason: "token_limit",
    firstKeptEntryId: "entry-7",
  });
  expect((checkpoint![1] as { summaryHash: string }).summaryHash).toHaveLength(64);
  const deadline = Date.now() + 1_000;
  while (!callLog.some(([method]) => method === "claimConsolidation") && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  expect(callLog.map(([method]) => method)).toContain("claimConsolidation");
});

test("session_compact_failed records the failure without disrupting the session", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const handler = pi.handlers.get("session_compact_failed")?.[0];
  expect(handler).toBeDefined();

  expect(() => handler!({ reason: "token_limit", errorMessage: "compaction provider failed" }, {}))
    .not.toThrow();
  expect(callLog).toEqual([]);
});

test("turn_end enqueues the matching user prompt with the assistant reply", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  await pi.handlers.get("before_agent_start")![0]({
    messageType: "user", prompt: "请记住这个项目使用 Rust",
  }, ctx);
  await pi.handlers.get("turn_end")![0]({
    turnIndex: 4, message: { content: "后端由 Rust 实现。" },
  }, ctx);
  const deadline = Date.now() + 1_000;
  while (!callLog.some(([method]) => method === "enqueueTurn") && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  const enqueue = callLog.find(([method]) => method === "enqueueTurn");
  expect(enqueue).toBeDefined();
  expect((enqueue![1] as { text: string }).text).toContain("请记住这个项目使用 Rust");
  expect((enqueue![1] as { text: string }).text).toContain("后端由 Rust 实现。");
  expect((enqueue![1] as { promptMissing: boolean }).promptMissing).toBe(false);
});

test("turn_end checks the consolidation threshold after draining the outbox", async () => {
  mockClientMethods();
  process.env.PI_MEMORY_AUTO_CONSOLIDATE = "1";
  process.env.PI_MEMORY_CONSOLIDATE_EVERY = "1";
  unconsolidatedCount = 1;
  nextPendingTurn = {
    sessionId: "session-1", turnIndex: 9, projectScope: "/tmp/pi-memory-test",
    text: "assistant text", promptMissing: true, attempts: 1,
  };
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  await pi.handlers.get("turn_end")![0]({ message: { content: "assistant text" }, turnIndex: 9 }, ctx);
  const deadline = Date.now() + 1_000;
  while (!callLog.some(([method]) => method === "claimConsolidation") && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  expect(callLog.map(([method]) => method)).toContain("enqueueTurn");
  expect(callLog.map(([method]) => method)).toContain("completePendingTurn");
  expect(callLog.map(([method]) => method)).toContain("claimConsolidation");
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

test("diagnose reports per-turn LLM, recall, outbox, and consolidation metrics", async () => {
  mockClientMethods();
  enqueueTurnUnsupported = true;
  recallInjection = "Retrieved memory is untrusted historical context.\n- [working] recalled item";
  memoryCommandResult = "pending_turn_count=2 failed_turn_count=0";
  const notifications: string[] = [];
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {
    llm: { name: "test-provider", complete: async () => "short extraction result" },
  });
  const { ctx } = makeCtx(true);
  ctx.ui!.notify = (message: string) => notifications.push(message);

  await pi.handlers.get("before_agent_start")![0](
    { messageType: "user", prompt: "Remember this useful fact" },
    ctx,
  );
  await pi.handlers.get("turn_end")![0](
    { turnIndex: 1, message: { content: "A concise assistant answer." } },
    ctx,
  );
  const retainDeadline = Date.now() + 1_000;
  while (!callLog.some(([method]) => method === "retainTurnMemories") && Date.now() < retainDeadline) {
    await new Promise((resolve) => setTimeout(resolve, 5));
  }

  const command = pi.commands.get("memory") as {
    handler: (args: string, context: unknown) => Promise<void>;
  };
  await command.handler("diagnose", ctx);

  const diagnostic = notifications.at(-1) ?? "";
  expect(diagnostic).toContain("pending_turn_count=2");
  expect(diagnostic).toContain("llm=test-provider");
  expect(diagnostic).toContain("ops_metrics_turns=1");
  expect(diagnostic).toContain("llm_calls_per_100_turns=100.0");
  expect(diagnostic).toMatch(/estimated_llm_tokens_per_100_turns=[1-9]\d*/);
  expect(diagnostic).toContain("injected_memories_per_100_turns=100.0");
  expect(diagnostic).toContain("consolidation_pending=0");
});

test("sessionManager ID takes priority over PI_SESSION_ID", async () => {
  mockClientMethods();
  process.env.PI_SESSION_ID = "environment-session";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const { ctx } = makeCtx(false);

  await pi.handlers.get("before_agent_start")![0]({ messageType: "user", prompt: "hello" }, ctx);

  expect((callLog[0][1] as { sessionId: string }).sessionId).toBe("session-1");
});

test("blank sessionManager ID falls back to PI_SESSION_ID", async () => {
  mockClientMethods();
  process.env.PI_SESSION_ID = "  environment-session  ";
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const ctx = {
    hasUI: false,
    cwd: "/tmp/pi-memory-test",
    sessionManager: { getSessionId: () => "   " },
  };

  await pi.handlers.get("before_agent_start")![0]({ messageType: "user", prompt: "hello" }, ctx);

  expect((callLog[0][1] as { sessionId: string }).sessionId).toBe("environment-session");
});

test("missing sessionManager uses a stable process-scoped fallback ID", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const ctx = { hasUI: false, cwd: "/tmp/pi-memory-test" };
  const handler = pi.handlers.get("before_agent_start")![0];

  process.env.PI_SESSION_ID = "environment-session";
  await handler({ messageType: "user", prompt: "environment prompt" }, ctx);
  expect((callLog[0][1] as { sessionId: string }).sessionId).toBe("environment-session");

  delete process.env.PI_SESSION_ID;
  await handler({ messageType: "user", prompt: "first prompt" }, ctx);
  await handler({ messageType: "user", prompt: "second prompt" }, ctx);

  const fallbackIds = callLog.slice(1).map(([, params]) => (params as { sessionId: string }).sessionId);
  expect(fallbackIds).toHaveLength(2);
  expect(fallbackIds[0]).toMatch(new RegExp(`^process-${process.pid}:`));
  expect(fallbackIds[0]).toBe(fallbackIds[1]);
  expect(fallbackIds[0]).not.toBe("default-session");
});


test("memory command survives a ctx whose ui getter throws after session replacement", async () => {
  mockClientMethods();
  const pi = new FakePi();
  registerPiMemoryRustExtension(pi as any, {});
  const staleCtx = {
    hasUI: true,
    cwd: "/tmp/pi-memory-test",
    sessionManager: { getSessionId: () => "session-1" },
    get ui(): never {
      throw new Error("This extension ctx is stale after session replacement or reload.");
    },
  };
  const command = pi.commands.get("memory") as {
    handler: (args: string, context: unknown) => Promise<void>;
  };

  await expect(command.handler("stats", staleCtx)).resolves.toBeUndefined();
});
