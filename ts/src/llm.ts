/**
 * 默认 LLM provider：通过 pi-ai 调用当前会话模型（pi 配置的 provider）。
 *
 * 设计：
 * - 开箱即用：直接复用 pi 当前模型（无需额外配置）；
 * - 可用 `PI_MEMORY_LLM=off` 关闭（保持零 API 成本的规则路径）；
 * - 失败由调用方降级规则提取（retain/consolidate 已有兜底）。
 */

import type { Model } from "@earendil-works/pi-ai";
import type { LlmProvider } from "../index.ts";

export interface PiAiLlmContext {
  /** 当前活跃模型（ctx.model） */
  model: Model;
  /** pi 的模型注册表（ctx.modelRegistry） */
  registry: {
    complete(model: Model, context: unknown): Promise<{ content: Array<{ type: string; text?: string }> }>;
  };
}

/** 是否启用 LLM（PI_MEMORY_LLM=off 关闭） */
export function llmEnabled(): boolean {
  return process.env.PI_MEMORY_LLM !== "off";
}

/** 用 pi-ai 构造默认 LLM provider（调用当前会话模型）。 */
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
      // 提取文本（过滤 thinking/tool-call 片段）
      const text = (result.content ?? [])
        .filter((part) => part.type === "text" && typeof part.text === "string")
        .map((part) => part.text ?? "")
        .join("");
      if (!text.trim()) throw new Error("LLM 返回空内容");
      return text;
    },
  };
}
