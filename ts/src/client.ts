/**
 * pi-memory-rust JSON-RPC client + resident process lifecycle management.
 *
 * Responsibilities (mirroring the oh-my-pi loader's separation of concerns):
 * - spawn/shutdown the resident Rust process (the `pi-memory-server` binary);
 * - frame protocol (`<u32 big-endian length><JSON>`) and version handshake;
 * - call timeout and error normalization;
 * - crash detection with bounded auto-restart;
 * - diagnostics (version, backend, recent errors).
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { createRequire } from "node:module";

/** Protocol version, kept in sync with the Rust `PROTOCOL_VERSION` */
export const PROTOCOL_VERSION = 1;

/** Default per-call timeout (ms); aligns with the TS embedding 100ms degradation target */
export const DEFAULT_CALL_TIMEOUT_MS = 2000;

export interface RpcError {
  code: number;
  message: string;
}

export interface RpcResponse {
  id: number;
  result?: unknown;
  error?: RpcError;
}

export interface ClientOptions {
  /** pi-memory-server binary path; resolved from env/relative paths when omitted */
  binaryPath?: string;
  callTimeoutMs?: number;
  /** Maximum restarts after a crash */
  maxRestarts?: number;
  /** Log output target (defaults to stderr) */
  log?: (line: string) => void;
}

export class MemoryRustClient {
  private proc: ChildProcessWithoutNullStreams | null = null;
  private startPromise: Promise<void> | null = null;
  private nextId = 1;
  private pending = new Map<number, { resolve: (r: RpcResponse) => void; reject: (error: Error) => void; timer: NodeJS.Timeout }>();
  private buffer = Buffer.alloc(0);
  private restartCount = 0;
  private shuttingDown = false;
  private startingUp = false;
  private readonly options: Required<ClientOptions>;

  constructor(options: ClientOptions = {}) {
    this.options = {
      binaryPath: options.binaryPath ?? resolveBinaryPath(),
      callTimeoutMs: options.callTimeoutMs ?? DEFAULT_CALL_TIMEOUT_MS,
      maxRestarts: options.maxRestarts ?? 3,
      log: options.log ?? ((line) => {
        // Quiet by default: writing child logs straight to stderr corrupts the pi TUI;
        // with PI_MEMORY_DEBUG=1, diagnostics go out (Rust content has no prefix; this adds one)
        if (process.env.PI_MEMORY_DEBUG) process.stderr.write(`[pi-memory-rust] ${line}\n`);
      }),
    };
  }

  /** Ensure the resident process is up and the version handshake completed */
  async ensureStarted(): Promise<void> {
    if (this.proc && this.proc.exitCode === null) return;
    if (this.startPromise) {
      await this.startPromise;
      return;
    }
    this.shuttingDown = false;
    this.startingUp = true;
    this.startPromise = (async () => {
      try {
        this.startProcess();
        const hello = await this.invoke("hello", { version: PROTOCOL_VERSION });
        if (hello.error) {
          throw new Error(`pi-memory-rust handshake failed: ${hello.error.message}`);
        }
      } catch (error) {
        const proc = this.proc;
        if (proc && proc.exitCode === null) {
          proc.kill("SIGKILL");
          await this.waitForExit(proc, 1000).catch(() => {});
        }
        throw error;
      } finally {
        this.startingUp = false;
      }
    })();
    try {
      await this.startPromise;
    } finally {
      this.startPromise = null;
    }
  }

  /** Generic call: returns result, throws on errors (timeouts, process exit) */
  async call<T = unknown>(method: string, params: unknown = {}, timeoutMs?: number): Promise<T> {
    await this.ensureStarted();
    const response = await this.invoke(method, params, timeoutMs);
    if (response.error) throw new Error(response.error.message);
    return response.result as T;
  }

  /** Health check (does not cascade errors when the process failed to start) */
  async diagnose(): Promise<Record<string, unknown>> {
    if (!this.proc || this.proc.exitCode !== null) {
      return { backend: "unavailable", error: "process not started" };
    }
    try {
      const response = await this.invoke("diagnose", {});
      if (response.error) return { backend: "error", error: response.error.message };
      return (response.result ?? {}) as Record<string, unknown>;
    } catch (error) {
      return { backend: "error", error: error instanceof Error ? error.message : String(error) };
    }
  }

  /** Graceful shutdown: send shutdown first, SIGKILL on timeout */
  async shutdown(): Promise<void> {
    this.shuttingDown = true;
    const proc = this.proc;
    if (!proc || proc.exitCode !== null) return;
    try {
      // The Rust side bounded-waits (≤800ms) for the background model load after responding; give it exit margin here
      await this.invoke("shutdown", {}, 500);
      await this.waitForExit(proc, 1500);
    } catch {
      proc.kill("SIGKILL");
      await this.waitForExit(proc, 1500).catch(() => {});
    } finally {
      if (this.proc === proc) {
        this.proc = null;
      }
    }
  }

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  private startProcess(resetRestartCount = true): void {
    if (resetRestartCount) {
      this.restartCount = 0;
    }
    // Database paths are managed by the Rust side per projectScope (single process, multiple DBs, physical isolation)
    if (!fs.existsSync(this.options.binaryPath)) {
      throw new Error(
        `pi-memory-server binary not found: ${this.options.binaryPath}\n` +
          `install the leaf package (@jackice/pi-memory-rust-${process.platform}-${process.arch}) or set PI_MEMORY_RUST_BINARY`,
      );
    }
    const proc = spawn(this.options.binaryPath, [], { stdio: ["pipe", "pipe", "pipe"] });
    this.proc = proc;
    proc.stdout.on("data", (chunk: Buffer) => this.onData(chunk));
    proc.stderr.on("data", (chunk: Buffer) => {
      this.options.log(chunk.toString().trimEnd());
    });
    proc.on("exit", (code, signal) => {
      this.onExit(code, signal);
    });
  }

  private onExit(code: number | null, signal: string | null): void {
    const reason = signal ? `signal ${signal}` : `exit code ${code}`;
    this.options.log(`process exited: ${reason}`);
    // Fail all pending requests
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(new Error(`process exited: ${reason}`));
    }
    this.pending.clear();
    this.proc = null;
    // Auto-restart on unexpected exit (bounded)
    if (!this.shuttingDown && !this.startingUp && this.restartCount < this.options.maxRestarts) {
      this.restartCount += 1;
      this.options.log(`auto-restart (#${this.restartCount})`);
      this.startProcess(false);
    }
  }

  private onData(chunk: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    // Parse complete frames
    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32BE(0);
      if (this.buffer.length < 4 + len) break;
      const payload = this.buffer.subarray(4, 4 + len).toString("utf8");
      this.buffer = this.buffer.subarray(4 + len);
      try {
        const response = JSON.parse(payload) as RpcResponse;
        const entry = this.pending.get(response.id);
        if (!entry) continue;
        clearTimeout(entry.timer);
        this.pending.delete(response.id);
        entry.resolve(response);
      } catch {
        this.options.log(`failed to parse response: ${payload.slice(0, 200)}`);
      }
    }
  }

  private invoke(method: string, params: unknown, timeoutMs?: number): Promise<RpcResponse> {
    const proc = this.proc;
    if (!proc) {
      return Promise.reject(new Error("pi-memory-rust process not started"));
    }
    const id = this.nextId++;
    const timer = setTimeout(() => {
      const entry = this.pending.get(id);
      if (entry) {
        this.pending.delete(id);
        entry.reject(new Error(`call timed out: ${method}`));
      }
      // Treat a timeout as an unresponsive process and restart like a crash
      proc.kill("SIGKILL");
      // The kill above triggers exit → fails the request; this is a backstop against hanging
    }, timeoutMs ?? this.options.callTimeoutMs);
    timer.unref();
    const promise = new Promise<RpcResponse>((resolve, reject) => {
      this.pending.set(id, { resolve, reject, timer });
    });
    const payload = JSON.stringify({ id, method, params });
    const frame = Buffer.alloc(4 + Buffer.byteLength(payload));
    frame.writeUInt32BE(Buffer.byteLength(payload), 0);
    frame.write(payload, 4, "utf8");
    proc.stdin.write(frame);
    return promise;
  }

  private waitForExit(proc: ChildProcessWithoutNullStreams, timeoutMs: number): Promise<void> {
    if (proc.exitCode !== null) {
      return Promise.resolve();
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        cleanup();
        reject(new Error("timed out waiting for process exit"));
      }, timeoutMs);
      timer.unref();

      const onExit = () => {
        cleanup();
        resolve();
      };

      const onError = (error: Error) => {
        cleanup();
        reject(error);
      };

      const cleanup = () => {
        clearTimeout(timer);
        proc.off("exit", onExit);
        proc.off("error", onError);
      };

      proc.once("exit", onExit);
      proc.once("error", onError);
    });
  }
}

/**
 * Resolve the pi-memory-server binary path:
 * 1. the PI_MEMORY_RUST_BINARY env var (explicit)
 * 2. the leaf package (npm installs, @jackice/pi-memory-rust-<platform>-<arch>)
 * 3. the in-package bin/ (local development, kept by pi install ./ts)
 */
function resolveBinaryPath(): string {
  const fromEnv = process.env.PI_MEMORY_RUST_BINARY;
  if (fromEnv) return fromEnv;
  try {
    const tag = `${process.platform}-${process.arch}`;
    const require = createRequire(import.meta.url);
    const leafDir = path.dirname(require.resolve(`@jackice/pi-memory-rust-${tag}/package.json`));
    const candidate = path.join(leafDir, "pi-memory-server" + (process.platform === "win32" ? ".exe" : ""));
    if (fs.existsSync(candidate)) return candidate;
  } catch {
    // Leaf package not installed (local dev/unpublished); fall back to the in-package bin
  }
  return new URL("../bin/pi-memory-server", import.meta.url).pathname;
}


