import { test, expect } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";

test("resident process lifecycle and JSON-RPC pipeline", async () => {
  const logs: string[] = [];
  const client = new MemoryRustClient({
    binaryPath: new URL("../bin/pi-memory-server", import.meta.url).pathname,
    log: (line) => logs.push(line),
  });

  // start + handshake
  await client.ensureStarted();

  // diagnose
  const diag = await client.diagnose();
  expect(diag.backend).toBe("turso");

  // unknown method should throw
  await expect(client.call("nope")).rejects.toThrow();

  // graceful shutdown
  await client.shutdown();
  expect(logs.some((line) => line.includes("SIGKILL"))).toBe(false);
  expect(logs.some((line) => line.includes("exit code 0"))).toBe(true);
});

