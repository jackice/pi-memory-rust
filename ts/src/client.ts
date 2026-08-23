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
import * as net from "node:net";
import * as os from "node:os";
import * as path from "node:path";
import { createRequire } from "node:module";

/** Protocol version, kept in sync with the Rust `PROTOCOL_VERSION` */
export const PROTOCOL_VERSION = 1;

/** Default per-call timeout (ms); aligns with the TS embedding 100ms degradation target */
export const DEFAULT_CALL_TIMEOUT_MS = 2000;

/** Max accepted frame size; mirrors the Rust read_frame cap (16 MiB) */
const MAX_FRAME_BYTES = 16 * 1024 * 1024;

/**
 * Unix socket 路径（单例模式：所有 pi 会话复用同一 server 进程）。
 * 与 Rust `paths::default_data_dir()` 保持一致，PI_MEMORY_DATA_DIR 可覆盖。
 */
export function socketPath(): string {
  const base = process.env.PI_MEMORY_DATA_DIR ?? path.join(os.homedir(), ".local", "share", "pi-memory-rust");
  return path.join(base, "pi-memory-rust.sock");
}

/** 单例模式是否启用（默认开启；PI_MEMORY_STDIO=1 强制 stdio 模式用于调试） */
export function socketModeEnabled(): boolean {
  return process.env.PI_MEMORY_STDIO !== "1";
}

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
  /** 单例 socket 模式（默认按 PI_MEMORY_STDIO 环境变量；显式传入可覆盖） */
  socketMode?: boolean;
}

export class MemoryRustClient {
  private proc: ChildProcessWithoutNullStreams | null = null;
  /** RPC 传输连接：socket 模式为 unix socket 连接（stdio 模式为 null，走 proc.stdio） */
  private conn: net.Socket | null = null;
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
      socketMode: options.socketMode ?? socketModeEnabled(),
      log: options.log ?? ((line) => {
        // Quiet by default: writing child logs straight to stderr corrupts the pi TUI;
        // with PI_MEMORY_DEBUG=1, diagnostics go out (Rust content has no prefix; this adds one)
        if (process.env.PI_MEMORY_DEBUG) process.stderr.write(`[pi-memory-rust] ${line}\n`);
      }),
    };
  }

  /** Ensure the resident process/connection is up and the version handshake completed */
  async ensureStarted(): Promise<void> {
    // Never auto-restart after a deliberate shutdown (avoids leaking orphan processes
    // when late async callbacks arrive after session teardown)
    if (this.shuttingDown) {
      throw new Error("pi-memory-rust client is shutting down");
    }
    if (this.isConnected()) return;
    if (this.startPromise) {
      await this.startPromise;
      return;
    }
    this.startingUp = true;
    this.startPromise = (async () => {
      try {
        await this.connectOrSpawn();
        const hello = await this.invoke("hello", { version: PROTOCOL_VERSION });
        if (hello.error) {
          throw new Error(`pi-memory-rust handshake failed: ${hello.error.message}`);
        }
      } catch (error) {
        this.teardown(error instanceof Error ? error.message : String(error));
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
    if (!this.isConnected()) {
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

  /** 优雅关闭：socket 模式发 shutdown 并断开连接；server 由自身生命周期管理（空闲退出/最后会话退出），不误杀其他会话共享的进程 */
  async shutdown(): Promise<void> {
    this.shuttingDown = true;
    const conn = this.conn;
    const proc = this.proc;
    if (!this.isConnected() && !proc) return;
    try {
      await this.invoke("shutdown", {}, 500);
    } catch {
      // 忽略：连接可能已断开，shutdown 是尽力而为
    } finally {
      this.conn?.destroy();
      this.conn = null;
      // 仅 stdio 模式需要等待进程退出（socket 模式下 server 生命周期自治）
      if (!this.options.socketMode && proc && proc.exitCode === null) {
        await this.waitForExit(proc, 1500).catch(() => {});
      }
    }
  }

  // -------------------------------------------------------------------------
  // Internals
  // -------------------------------------------------------------------------

  /** 是否已有可用连接（socket 模式看 conn，stdio 模式看 proc） */
  private isConnected(): boolean {
    if (this.options.socketMode) {
      return this.conn !== null && !this.conn.destroyed;
    }
    return this.proc !== null && this.proc.exitCode === null;
  }

  /** 连接或 spawn：socket 模式先尝试复用已有 server，失败再 spawn；stdio 模式直接 spawn */
  private async connectOrSpawn(): Promise<void> {
    if (!this.options.socketMode) {
      this.startProcess(true);
      return;
    }
    const sock = socketPath();
    try {
      this.conn = await this.connectSocket(sock, 300); // 短超时：快速失败则 spawn
      this.attachConn(this.conn);
      return;
    } catch {
      // 无已有 server（或暂时不可用）：spawn 一个新进程
    }
    this.startProcess(true);
    try {
      this.conn = await this.connectSocket(sock, 3000); // spawn 后等待 server bind
      this.attachConn(this.conn);
    } catch (error) {
      throw new Error(`pi-memory-rust failed to connect to server: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  /** 连接 unix socket，带重试（server 启动需要时间 bind） */
  private connectSocket(sock: string, timeoutMs: number): Promise<net.Socket> {
    return new Promise((resolve, reject) => {
      const deadline = Date.now() + timeoutMs;
      const attempt = () => {
        // 先创建 Socket 并注册监听，再 connect：bun 的 net 对 ENOENT 可能在
        // 监听器注册前就触发 error（unhandled），必须先注册后连接
        const socket = new net.Socket();
        const cleanup = () => {
          clearTimeout(timer);
          socket.removeListener("connect", onConnect);
          socket.removeListener("error", onError);
        };
        const onConnect = () => {
          cleanup();
          resolve(socket);
        };
        const onError = (error: Error) => {
          cleanup();
          socket.destroy();
          if (Date.now() < deadline) {
            setTimeout(attempt, 50);
          } else {
            reject(error);
          }
        };
        const timer = setTimeout(() => {
          cleanup();
          socket.destroy();
          if (Date.now() < deadline) {
            attempt();
          } else {
            reject(new Error(`connect timeout (${timeoutMs}ms)`));
          }
        }, 200);
        socket.once("connect", onConnect);
        socket.once("error", onError);
        socket.connect(sock);
      };
      attempt();
    });
  }

  /** 挂接连接事件（帧解析 + 断开处理） */
  private attachConn(conn: net.Socket): void {
    conn.on("data", (chunk: Buffer) => this.onData(chunk));
    conn.on("error", () => {}); // 连接错误由 close 统一处理
    conn.on("close", () => {
      if (this.conn === conn) {
        this.conn = null;
        this.onDisconnected("connection closed");
      }
    });
  }

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
    const args = this.options.socketMode ? ["--socket", socketPath()] : [];
    const proc = spawn(this.options.binaryPath, args, { stdio: ["pipe", "pipe", "pipe"] });
    this.proc = proc;
    // Swallow pipe errors (EPIPE etc.): a process can exit right after we write a frame,
    // and an unhandled 'error' event on stdin would throw inside the host
    proc.stdin.on("error", () => {});
    proc.on("error", (error) => {
      // Spawn failure (e.g. the binary was removed between the existsSync check and spawn):
      // fail pending requests and do not auto-restart (retrying a broken binary is pointless)
      this.options.log(`process error: ${error.message}`);
      for (const [, entry] of this.pending) {
        clearTimeout(entry.timer);
        entry.reject(new Error(`process error: ${error.message}`));
      }
      this.pending.clear();
      this.proc = null;
    });
    if (!this.options.socketMode) {
      proc.stdout.on("data", (chunk: Buffer) => this.onData(chunk));
    }
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
    this.proc = null;
    if (this.conn) {
      this.conn.destroy(); // close 事件会触发 onDisconnected（统一处理 pending + 重连）
    } else {
      // stdio 模式：无 socket 连接，直接走断开处理（含自动重启）
      this.onDisconnected(`process exited: ${reason}`);
    }
  }

  /** 连接断开/进程退出：失败所有挂起请求，非主动关闭时尝试重连/重启 */
  /** 启动失败/握手失败：清理连接与进程（不触发重连） */
  private teardown(reason: string): void {
    this.options.log(`teardown: ${reason}`);
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(new Error(reason));
    }
    this.pending.clear();
    this.conn?.destroy();
    this.conn = null;
    if (this.proc && this.proc.exitCode === null) {
      this.proc.kill("SIGKILL");
    }
    this.proc = null;
  }

  private onDisconnected(reason: string): void {
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(new Error(reason));
    }
    this.pending.clear();
    if (this.shuttingDown || this.startingUp || this.restartCount >= this.options.maxRestarts) {
      return;
    }
    // 有限次重连：先尝试复用（server 可能空闲重启中），失败再 spawn
    this.restartCount += 1;
    this.options.log(`reconnecting (#${this.restartCount})`);
    this.connectOrSpawn().catch((error) => {
      this.options.log(`reconnect failed: ${error instanceof Error ? error.message : String(error)}`);
    });
  }

  private onData(chunk: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    // Parse complete frames
    while (this.buffer.length >= 4) {
      const len = this.buffer.readUInt32BE(0);
      if (len > MAX_FRAME_BYTES) {
        // A malformed oversized frame means the wire protocol is broken;
        // drop the buffer and reset the transport (socket: reconnect; stdio: kill process)
        this.options.log(`oversized frame (${len} bytes) — resetting transport`);
        this.buffer = Buffer.alloc(0);
        if (this.options.socketMode) {
          this.conn?.destroy();
        } else if (this.proc && this.proc.exitCode === null) {
          this.proc.kill("SIGKILL");
        }
        return;
      }
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
    const transport = this.options.socketMode ? this.conn : this.proc;
    if (!transport) {
      return Promise.reject(new Error("pi-memory-rust process not started"));
    }
    const id = this.nextId++;
    const timer = setTimeout(() => {
      const entry = this.pending.get(id);
      if (entry) {
        this.pending.delete(id);
        entry.reject(new Error(`call timed out: ${method}`));
      }
      // 超时视为无响应：断开连接触发重连（stdio 模式 kill 进程）
      if (this.options.socketMode) {
        this.conn?.destroy();
      } else if (this.proc && this.proc.exitCode === null) {
        this.proc.kill("SIGKILL");
      }
    }, timeoutMs ?? this.options.callTimeoutMs);
    timer.unref();
    const promise = new Promise<RpcResponse>((resolve, reject) => {
      this.pending.set(id, { resolve, reject, timer });
    });
    const payload = JSON.stringify({ id, method, params });
    const frame = Buffer.alloc(4 + Buffer.byteLength(payload));
    frame.writeUInt32BE(Buffer.byteLength(payload), 0);
    frame.write(payload, 4, "utf8");
    if (this.options.socketMode) {
      (transport as net.Socket).write(frame);
    } else {
      (transport as ChildProcessWithoutNullStreams).stdin.write(frame);
    }
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


