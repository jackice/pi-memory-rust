/**
 * Default LLM provider: calls the current session model through pi-ai (pi's configured provider).
 *
 * Design:
 * - works out of the box: reuses pi's current model (no extra config);
 * - `PI_MEMORY_LLM=off` disables it (keeping the zero-API-cost rule path);
 * - failures degrade to rule extraction in the caller (retain/consolidate already fall back).
 */

import type { Model } from "@earendil-works/pi-ai";
import type { LlmProvider } from "../index.ts";

export interface PiAiLlmContext {
  /** The current active model (ctx.model) */
  model: Model;
  /** pi's model registry (ctx.modelRegistry) */
  registry: {
    complete(model: Model, context: unknown): Promise<{ content: Array<{ type: string; text?: string }> }>;
  };
}

/** Whether LLM is enabled (PI_MEMORY_LLM=off disables it) */
export function llmEnabled(): boolean {
  return process.env.PI_MEMORY_LLM !== "off";
}

/** Build the default LLM provider with pi-ai (calls the current session model). */
export function createPiAiLlmProvider(ctx: PiAiLlmContext): LlmProvider {
  return {
    name: `pi-ai:${ctx.model.provider}/${ctx.model.id}`,
    async complete(prompt: string): Promise<string> {
      const result = await ctx.registry.complete(ctx.model, {
        systemPrompt: undefined,
        messages: [
          {
            role: "user",
            content: prompt,
            timestamp: Date.now(),
          },
        ],
      });
      // Extract text (filter out thinking/tool-call parts)
      const text = (result.content ?? [])
        .filter((part) => part.type === "text" && typeof part.text === "string")
        .map((part) => part.text ?? "")
        .join("");
      if (!text.trim()) throw new Error("LLM returned empty content");
      return text;
    },
  };
}

