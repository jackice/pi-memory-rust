/**
 * pi-memory-rust pi 扩展入口（M1 骨架）。
 *
 * 职责：绑定 pi 事件（before_agent_start / turn_end / session_shutdown）、
 * 注册 /memory 命令、管理 Rust 常驻进程生命周期（通过 MemoryRustClient）。
 * 业务逻辑全部在 Rust 子进程内，此处仅做桥接与降级。
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { MemoryRustClient } from "./src/client.ts";

export default function registerPiMemoryRustExtension(pi: ExtensionAPI): void {
  const client = new MemoryRustClient();

  pi.on("session_start", async () => {
    try {
      await client.ensureStarted();
    } catch (error) {
      notify(pi, "recall", error);
    }
  });

  pi.on("session_shutdown", async () => {
    await client.shutdown().catch(() => {});
  });

  pi.on("before_agent_start", async (event, ctx) => {
    try {
      const injection = await client.call<string>("buildRecallInjection", {
        prompt: event.prompt,
        limit: 10,
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
      notify(ctx as { ui?: { notify?: (m: string, l?: string) => void } } | undefined, "recall", error);
    }
  });

  pi.on("turn_end", async (event, ctx) => {
    try {
      await client.call("retainTurnMemories", {
        turnIndex: event.turnIndex,
        text: extractText(event.message),
      });
    } catch (error) {
      notify(ctx as { ui?: { notify?: (m: string, l?: string) => void } } | undefined, "retain", error);
    }
  });

  pi.registerCommand("memory", {
    description: "pi-memory-rust 记忆维护（view/stats/diagnose/clear/rebuild）",
    handler: async (args, ctx) => {
      const [sub, ...rest] = (args ?? "").split(/\s+/).filter(Boolean);
      const target = rest[0] ?? "";
      try {
        const result = await client.call<string>("memoryCommand", { sub: sub ?? "view", target });
        ctx.ui.notify(result, "info");
      } catch (error) {
        notify(ctx, "command", error);
      }
    },
  });
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

function notify(
  ctx: { ui?: { notify?: (message: string, level?: string) => void } } | undefined,
  phase: string,
  error: unknown,
): void {
  const message = error instanceof Error ? error.message : String(error);
  ctx?.ui?.notify?.(`pi-memory-rust ${phase} degraded: ${message}`, "warning");
}
