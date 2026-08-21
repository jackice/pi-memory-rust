import { test, expect } from "bun:test";
import { createPiAiLlmProvider, llmEnabled } from "../src/llm.ts";

function fakeRegistry(text: string) {
  return {
    complete: async () => ({
      content: [
        { type: "thinking" as const, thinking: "内部推理" },
        { type: "text" as const, text },
      ],
    }),
  };
}

test("pi-ai provider 提取文本（跳过 thinking 片段）", async () => {
  const provider = createPiAiLlmProvider({
    model: { id: "test-model", name: "Test", provider: "test", api: {} as never } as never,
    registry: fakeRegistry('{"facts": [{"subject": "项目", "predicate": "采用", "object": "Rust"}]}') as never,
  });
  const result = await provider.complete("提取结构化记忆");
  expect(result).toContain("Rust");
  expect(result).not.toContain("thinking");
  expect(result).not.toContain("内部推理");
});

test("llmEnabled 受 PI_MEMORY_LLM 环境变量控制", () => {
  delete process.env.PI_MEMORY_LLM;
  expect(llmEnabled()).toBe(true); // 默认启用

  process.env.PI_MEMORY_LLM = "off";
  expect(llmEnabled()).toBe(false);

  process.env.PI_MEMORY_LLM = "on";
  expect(llmEnabled()).toBe(true);
  delete process.env.PI_MEMORY_LLM;
});

test("provider 空内容时抛错（调用方降级）", async () => {
  const provider = createPiAiLlmProvider({
    model: { id: "m", provider: "p", api: {} as never } as never,
    registry: fakeRegistry("   ") as never,
  });
  await expect(provider.complete("x")).rejects.toThrow();
});
