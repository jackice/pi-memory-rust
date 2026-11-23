import { expect, test } from "bun:test";
import { MemoryRustClient } from "../src/client.ts";

function decodeRequest(frame: Buffer): { id: number; method: string } {
  const length = frame.readUInt32BE(0);
  return JSON.parse(frame.subarray(4, 4 + length).toString("utf8"));
}

function encodeResponse(id: number, result: unknown): Buffer {
  const payload = Buffer.from(JSON.stringify({ id, result }));
  const frame = Buffer.alloc(4 + payload.length);
  frame.writeUInt32BE(payload.length, 0);
  payload.copy(frame, 4);
  return frame;
}

async function waitForWrites(writes: Buffer[], count: number): Promise<void> {
  const deadline = Date.now() + 100;
  while (writes.length < count && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
  expect(writes.length).toBeGreaterThanOrEqual(count);
}

function installSocketTransport(client: MemoryRustClient): {
  writes: Buffer[];
  state: { destroyed: boolean };
  deliver: (id: number, result: unknown) => void;
} {
  const writes: Buffer[] = [];
  const state = { destroyed: false };
  const fakeSocket = {
    destroyed: false,
    write: (frame: Buffer) => { writes.push(Buffer.from(frame)); return true; },
    destroy: () => { state.destroyed = true; fakeSocket.destroyed = true; },
  };
  (client as any).conn = fakeSocket;
  return {
    writes,
    state,
    deliver: (id, result) => (client as any).onData(encodeResponse(id, result)),
  };
}

test("one socket RPC timeout leaves unrelated pending calls and transport intact", async () => {
  const client = new MemoryRustClient({ socketMode: true, log: () => {} });
  const transport = installSocketTransport(client);
  const slow = client.call("slow", {}, 20);
  const fast = client.call("fast", {}, 500);
  await waitForWrites(transport.writes, 2);
  const slowRequest = decodeRequest(transport.writes[0]);
  const fastRequest = decodeRequest(transport.writes[1]);

  await expect(slow).rejects.toThrow("call timed out: slow");
  expect(transport.state.destroyed).toBe(false);
  transport.deliver(fastRequest.id, { ok: true });

  await expect(fast).resolves.toEqual({ ok: true });
  expect(slowRequest.id).not.toBe(fastRequest.id);
  expect((client as any).pending.size).toBe(0);
  expect(transport.state.destroyed).toBe(false);
});

test("late socket response is ignored without stealing a later response", async () => {
  const client = new MemoryRustClient({ socketMode: true, log: () => {} });
  const transport = installSocketTransport(client);
  const slow = client.call("slow", {}, 20);
  await waitForWrites(transport.writes, 1);
  const slowRequest = decodeRequest(transport.writes[0]);
  await expect(slow).rejects.toThrow("call timed out: slow");

  transport.deliver(slowRequest.id, "late result");
  const next = client.call("next", {}, 500);
  await waitForWrites(transport.writes, 2);
  const nextRequest = decodeRequest(transport.writes[1]);
  transport.deliver(slowRequest.id, "duplicate late result");
  transport.deliver(nextRequest.id, "next result");

  await expect(next).resolves.toBe("next result");
  expect(transport.state.destroyed).toBe(false);
  expect((client as any).pending.size).toBe(0);
});

test("stdio RPC timeout does not kill its dedicated server process", async () => {
  const writes: Buffer[] = [];
  let killed = false;
  const fakeProcess = {
    exitCode: null,
    stdin: { write: (frame: Buffer) => { writes.push(Buffer.from(frame)); return true; } },
    kill: () => { killed = true; return true; },
  };
  const client = new MemoryRustClient({ socketMode: false, log: () => {} });
  (client as any).proc = fakeProcess;

  const slow = client.call("slow", {}, 20);
  await waitForWrites(writes, 1);
  await expect(slow).rejects.toThrow("call timed out: slow");
  expect(killed).toBe(false);
  expect(writes).toHaveLength(1);
  expect(decodeRequest(writes[0]).method).toBe("slow");
});
