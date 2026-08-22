/**
 * pi-memory-rust pi extension entry point (M3).
 *
 * Responsibilities: bind pi events, register the /memory command, manage the Rust resident process, LLM proxy.
 * LLM proxy: an optional LlmProvider (built on pi-ai's n() or an external service);
 * when present, it produces llmResult for retain/consolidate (Rust parses it or falls back to rules).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { Model } from "@earendil-works/pi-ai";
import { MemoryRustClient } from "./src/client.ts";
import { createPiAiLlmProvider, llmEnabled, type PiAiLlmContext } from "./src/llm.ts";

/** LLM provider interface (aligned with the pi-memory v0.1.2 LlmProvider). */
export interface LlmProvider {
  name: string;
  complete(prompt: string): Promise<string>;
}

interface UiLike {
  notify?: (message: string, level?: string) => void;
  confirm?: (title: string, message: string, opts?: { timeout?: number; signal?: AbortSignal }) => Promise<boolean>;
}

interface CtxLike {
  cwd?: string;
  hasUI?: boolean;
  sessionManager?: { getSessionId?: () => string };
  ui?: UiLike;
}

export interface PiMemoryRustOptions {
  /** LLM provider; when absent, retain/consolidate use the rule path (llm=off) */
  llm?: LlmProvider;
}

export default function registerPiMemoryRustExtension(
  pi: ExtensionAPI,
  options: PiMemoryRustOptions = {},
): void {
  const client = new MemoryRustClient();
  let prompts: { extract: string; consolidate: string } | null = null;

  async function ensurePrompts(timeoutMs?: number): Promise<void> {
    if (prompts) return;
    prompts = await client.call("llmPrompts", {}, timeoutMs);
  }

  const SHUTDOWN_CONFIRM_TIMEOUT_MS = 1500;
  const SHUTDOWN_LLM_TIMEOUT_MS = 1000;
  // turn_end extraction uses a generous timeout: 1s would make mainstream models nearly always
  // time out and silently degrade every turn to the rule path; only the shutdown flow uses SHUTDOWN_LLM_TIMEOUT_MS
  const TURN_LLM_TIMEOUT_MS = 60_000;

  function hasShutdownUi(ctx: CtxLike | undefined): ctx is CtxLike & { hasUI: true; ui: Required<Pick<UiLike, "confirm">> } {
    return ctx?.hasUI !== false && typeof ctx?.ui?.confirm === "function";
  }

  async function shouldSummarizeOnShutdown(event: { reason?: string }, ctx: CtxLike | undefined): Promise<boolean> {
    if (event.reason !== "quit") return false;
    if (!hasShutdownUi(ctx)) return false;
    try {
      return await ctx.ui.confirm(
        "Summarize this session before exiting?",
        "Summarizing consolidates this session's working memories and may take a moment.",
        { timeout: SHUTDOWN_CONFIRM_TIMEOUT_MS },
      );
    } catch {
      return false;
    }
  }

  async function withTimeout<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
    return await new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${label} timed out`)), timeoutMs);
      timer.unref();
      promise.then(
        (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        (error) => {
          clearTimeout(timer);
          reject(error);
        },
      );
    });
  }

  /**
   * Resolve the LLM provider:
   * 1. explicit injection (options.llm);
   * 2. default to pi-ai (the current session model); `PI_MEMORY_LLM=off` disables it (staying on the rule path);
   * 3. returns undefined without an available model (degrading to the rule path).
   */
  function resolveLlm(ctx: unknown): LlmProvider | undefined {
    if (options.llm) return options.llm;
    if (!llmEnabled()) return undefined;
    const c = ctx as { model?: Model; modelRegistry?: PiAiLlmContext["registry"] } | undefined;
    if (process.env.PI_MEMORY_LLM_DEBUG) {
      console.error(`[pi-memory-llm] model=${c?.model ? c.model.id : "undefined"} registry=${c?.modelRegistry ? "ok" : "undefined"}`);
    }
    if (!c?.model || !c?.modelRegistry) return undefined;
    return createPiAiLlmProvider({ model: c.model, registry: c.modelRegistry });
  }

  async function runLlm(prompt: string, ctx: unknown, timeoutMs = TURN_LLM_TIMEOUT_MS): Promise<{ result?: string; failed: boolean }> {
    const provider = resolveLlm(ctx);
    if (!provider) return { failed: false };
    try {
      return { result: await withTimeout(provider.complete(prompt), timeoutMs, "LLM"), failed: false };
    } catch {
      return { failed: true };
    }
  }

  pi.on("session_start", async () => {
    try {
      await client.ensureStarted();
    } catch (error) {
      notify(pi, "recall", error);
    }
  });

  pi.on("session_shutdown", async (event, ctx) => {
    try {
      if (await shouldSummarizeOnShutdown(event, ctx as CtxLike | undefined)) {
        await ensurePrompts(SHUTDOWN_LLM_TIMEOUT_MS);
        // LLM input: this session's un-consolidated working content
        const source = await client.call<string>("consolidateSource", {
          sessionId: resolveSessionId(ctx),
          projectScope: resolveProjectScope(ctx),
        }, SHUTDOWN_LLM_TIMEOUT_MS);
        const { result, failed } = await runLlm(`${prompts?.consolidate ?? ""}\n\n${source}`, ctx, SHUTDOWN_LLM_TIMEOUT_MS);
        await client.call("consolidateSessionMemories", {
          sessionId: resolveSessionId(ctx),
          projectScope: resolveProjectScope(ctx),
          llmResult: result,
          llmFailed: failed,
        }, SHUTDOWN_LLM_TIMEOUT_MS);
      }
    } catch (error) {
      notify(ctx as CtxLike | undefined, "consolidate", error);
    } finally {
      await client.shutdown().catch(() => {});
    }
  });

  pi.on("before_agent_start", async (event, ctx) => {
    try {
      const injection = await client.call<string>("buildRecallInjection", {
        prompt: event.prompt,
        sessionId: resolveSessionId(ctx),
        projectScope: resolveProjectScope(ctx),
      });
      if (!injection) return;
      return {
        message: {
          customType: "pi-memory-rust-recall",
          content: injection,
          display: true,
        },
      };
    } catch (error) {
      notify(ctx as CtxLike | undefined, "recall", error);
    }
  });

  pi.on("turn_end", async (event, ctx) => {
    const text = extractText(event.message);
    if (!text) return;
    try {
      await ensurePrompts();
      const { result, failed } = await runLlm(`${prompts?.extract ?? ""}\n\n${text}`, ctx);
      await client.call("retainTurnMemories", {
        turnIndex: event.turnIndex,
        text,
        sessionId: resolveSessionId(ctx),
        projectScope: resolveProjectScope(ctx),
        llmResult: result,
        llmFailed: failed,
      });
    } catch (error) {
      notify(ctx as CtxLike | undefined, "retain", error);
    }
  });

  pi.registerCommand("memory", {
    description: "pi-memory-rust memory maintenance (view/stats/diagnose/clear/rebuild/migrate/prepare-model)",
    handler: async (args, ctx) => {
      const [sub, ...rest] = (args ?? "").split(/\s+/).filter(Boolean);
      try {
        let result: string;
        if (sub === "prepare-model") {
          result = await client.call<string>("prepareModel");
        } else if (sub === "migrate" && rest[0]) {
          result = JSON.stringify(await client.call("migrate", { sourcePath: rest.join(" ") }));
        } else {
          result = await client.call<string>("memoryCommand", {
            sub: sub ?? "view",
            target: rest.join(" "),
            sessionId: resolveSessionId(ctx),
            projectScope: resolveProjectScope(ctx),
          });
        }
        // Attach the real TS-side LLM status for diagnose (the Rust side cannot see pi's provider)
        if (sub === "diagnose") {
          const llm = resolveLlm(ctx);
          result += ` llm=${llm ? llm.name : "off"}`;
        }
        ctx.ui.notify(result, "info");
      } catch (error) {
        notify(ctx, "command", error);
      }
    },
  });
}

function resolveProjectScope(ctx: unknown): string {
  return (ctx as { cwd?: string } | undefined)?.cwd ?? process.cwd();
}

function resolveSessionId(ctx: unknown): string {
  const sessionManager = (ctx as { sessionManager?: { getSessionId?: () => string } } | undefined)?.sessionManager;
  return sessionManager?.getSessionId?.() ?? process.env.PI_SESSION_ID ?? "default-session";
}

function extractText(message: unknown): string {
  const content = (message as { content?: unknown } | undefined)?.content;
  if (!content) return "";
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter((part): part is { type?: string; text?: string } => typeof part === "object" && part !== null)
    .filter((part) => part.type === "text")
    .map((part) => part.text ?? "")
    .join("\n");
}

function notify(ctx: { ui?: UiLike } | undefined, phase: string, error: unknown): void {
  const message = error instanceof Error ? error.message : String(error);
  ctx?.ui?.notify?.(`pi-memory-rust ${phase} degraded: ${message}`, "warning");
}

