/**
 * pi-memory-rust pi 扩展入口（M3）。
 *
 * 职责：绑定 pi 事件、注册 /memory 命令、管理 Rust 常驻进程、LLM 代理。
 * LLM 代理：可选注入 LlmProvider（可基于 pi-ai 的 n() 或外部服务实现），
 * 有则 retain/consolidate 前生成 llmResult 传给 Rust（Rust 解析或降级规则路径）。
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { Model } from "@earendil-works/pi-ai";
import { MemoryRustClient } from "./src/client.ts";
import { createPiAiLlmProvider, llmEnabled, type PiAiLlmContext } from "./src/llm.ts";

/** LLM provider 接口（与 pi-memory v0.1.2 的 LlmProvider 对齐）。 */
export interface LlmProvider {
  name: string;
  complete(prompt: string): Promise<string>;
}

interface UiLike {
  notify?: (message: string, level?: string) => void;
}

interface CtxLike {
  cwd?: string;
  sessionManager?: { getSessionId?: () => string };
  ui?: UiLike;
}

export interface PiMemoryRustOptions {
  /** LLM 提供者；缺省时 retain/consolidate 走规则路径（llm=off） */
  llm?: LlmProvider;
}

export default function registerPiMemoryRustExtension(
  pi: ExtensionAPI,
  options: PiMemoryRustOptions = {},
): void {
  const client = new MemoryRustClient();
  let prompts: { extract: string; consolidate: string } | null = null;

  async function ensurePrompts(): Promise<void> {
    if (prompts) return;
    prompts = await client.call("llmPrompts");
  }

  /**
   * 解析 LLM provider：
   * 1. 显式注入（options.llm）；
   * 2. 默认用 pi-ai（当前会话模型），`PI_MEMORY_LLM=off` 可关闭（保持规则路径）；
   * 3. 无可用模型时返回 undefined（降级规则路径）。
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

  async function runLlm(prompt: string, ctx: unknown): Promise<{ result?: string; failed: boolean }> {
    const provider = resolveLlm(ctx);
    if (!provider) return { failed: false };
    try {
      return { result: await provider.complete(prompt), failed: false };
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
      await ensurePrompts();
      // LLM 输入：当前 session 未 consolidated 的 working 内容
      const source = await client.call<string>("consolidateSource", {
        sessionId: resolveSessionId(ctx),
        projectScope: resolveProjectScope(ctx),
      });
      const { result, failed } = await runLlm(`${prompts?.consolidate ?? ""}\n\n${source}`, ctx);
      await client.call("consolidateSessionMemories", {
        sessionId: resolveSessionId(ctx),
        projectScope: resolveProjectScope(ctx),
        llmResult: result,
        llmFailed: failed,
      });
    } catch (error) {
      notify(ctx as CtxLike | undefined, "consolidate", error);
    }
    await client.shutdown().catch(() => {});
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
    description: "pi-memory-rust 记忆维护（view/stats/diagnose/clear/rebuild/migrate/prepare-model）",
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
        // diagnose 时附上 TS 侧真实 LLM 状态（Rust 侧无法感知 pi 的 provider）
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
