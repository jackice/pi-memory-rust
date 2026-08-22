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

test("pi-ai provider extracts text (skipping thinking parts)", async () => {
  const provider = createPiAiLlmProvider({
    model: { id: "test-model", name: "Test", provider: "test", api: {} as never } as never,
    registry: fakeRegistry('{"facts": [{"subject": "项目", "predicate": "采用", "object": "Rust"}]}') as never,
  });
  const result = await provider.complete("extract structured memories");
  expect(result).toContain("Rust");
  expect(result).not.toContain("thinking");
  expect(result).not.toContain("内部推理");
});

test("llmEnabled is controlled by the PI_MEMORY_LLM env var", () => {
  delete process.env.PI_MEMORY_LLM;
  expect(llmEnabled()).toBe(true); // enabled by default

  process.env.PI_MEMORY_LLM = "off";
  expect(llmEnabled()).toBe(false);

  process.env.PI_MEMORY_LLM = "on";
  expect(llmEnabled()).toBe(true);
  delete process.env.PI_MEMORY_LLM;
});

test("provider throws on empty content (caller degrades)", async () => {
  const provider = createPiAiLlmProvider({
    model: { id: "m", provider: "p", api: {} as never } as never,
    registry: fakeRegistry("   ") as never,
  });
  await expect(provider.complete("x")).rejects.toThrow();
});

