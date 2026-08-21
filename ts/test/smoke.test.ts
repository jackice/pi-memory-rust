import { test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";

test("常驻进程生命周期与 JSON-RPC 链路", async () => {
  const client = new MemoryRustClient({ binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname });

  // 启动 + 握手
  await client.ensureStarted();

  // diagnose
  const diag = await client.diagnose();
  expect(diag.backend).toBe("turso");

  // 未知方法应抛错
  await expect(client.call("nope")).rejects.toThrow();

  // 优雅关闭
  await client.shutdown();
});
