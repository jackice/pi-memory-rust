/**
 * pi-memory-rust JSON-RPC 客户端 + 常驻进程生命周期管理。
 *
 * 职责（参照 oh-my-pi loader 的职责分离思路）：
 * - spawn / shutdown 常驻 Rust 进程（`pi-memory-server` 二进制）；
 * - 帧协议（`<u32 大端长度><JSON>`）与版本握手；
 * - 调用超时与错误归一化；
 * - 崩溃检测与有限次自动重启；
 * - 诊断信息（版本、后端、最近错误）。
 */

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";
import { createRequire } from "node:module";

/** 协议版本，与 Rust 侧 `PROTOCOL_VERSION` 保持一致 */
export const PROTOCOL_VERSION = 1;

/** 单次调用默认超时（ms），对齐 TS 版 embedding 100ms 语义的降级目标 */
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
  /** pi-memory-server 二进制路径；缺省时按环境变量/相对路径解析 */
  binaryPath?: string;
  callTimeoutMs?: number;
  /** 进程崩溃后的最大重启次数 */
  maxRestarts?: number;
  /** 日志输出目标（默认 stderr） */
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
      log: options.log ?? ((line) => process.stderr.write(`[pi-memory-rust] ${line}\n`)),
    };
  }

  /** 确保常驻进程已启动并完成版本握手 */
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
          throw new Error(`pi-memory-rust 协议握手失败: ${hello.error.message}`);
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

  /** 通用调用：返回 result，错误（含超时/进程退出）抛异常 */
  async call<T = unknown>(method: string, params: unknown = {}, timeoutMs?: number): Promise<T> {
    await this.ensureStarted();
    const response = await this.invoke(method, params, timeoutMs);
    if (response.error) throw new Error(response.error.message);
    return response.result as T;
  }

  /** 健康自检（不会触发进程启动失败时的级联错误） */
  async diagnose(): Promise<Record<string, unknown>> {
    if (!this.proc || this.proc.exitCode !== null) {
      return { backend: "unavailable", error: "进程未启动" };
    }
    try {
      const response = await this.invoke("diagnose", {});
      if (response.error) return { backend: "error", error: response.error.message };
      return (response.result ?? {}) as Record<string, unknown>;
    } catch (error) {
      return { backend: "error", error: error instanceof Error ? error.message : String(error) };
    }
  }

  /** 优雅关闭：先发 shutdown，超时则强杀 */
  async shutdown(): Promise<void> {
    this.shuttingDown = true;
    const proc = this.proc;
    if (!proc || proc.exitCode !== null) return;
    try {
      await this.invoke("shutdown", {}, 1000);
      await this.waitForExit(proc, 1000);
    } catch {
      proc.kill("SIGKILL");
      await this.waitForExit(proc, 1000).catch(() => {});
    } finally {
      if (this.proc === proc) {
        this.proc = null;
      }
    }
  }

  // -------------------------------------------------------------------------
  // 内部
  // -------------------------------------------------------------------------

  private startProcess(resetRestartCount = true): void {
    if (resetRestartCount) {
      this.restartCount = 0;
    }
    // 数据库路径由 Rust 侧按 projectScope 管理（单进程多库，物理隔离）
    if (!fs.existsSync(this.options.binaryPath)) {
      throw new Error(
        `pi-memory-server 二进制不存在: ${this.options.binaryPath}\n` +
          `请安装 leaf 包（@jackice/pi-memory-rust-${process.platform}-${process.arch}）或设置 PI_MEMORY_RUST_BINARY`,
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
    const reason = signal ? `信号 ${signal}` : `退出码 ${code}`;
    this.options.log(`进程退出：${reason}`);
    // 失败所有挂起请求
    for (const [, entry] of this.pending) {
      clearTimeout(entry.timer);
      entry.reject(new Error(`进程退出：${reason}`));
    }
    this.pending.clear();
    this.proc = null;
    // 非主动关闭时自动重启（有限次）
    if (!this.shuttingDown && !this.startingUp && this.restartCount < this.options.maxRestarts) {
      this.restartCount += 1;
      this.options.log(`自动重启（第 ${this.restartCount} 次）`);
      this.startProcess(false);
    }
  }

  private onData(chunk: Buffer): void {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    // 解析完整的帧
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
        this.options.log(`响应解析失败: ${payload.slice(0, 200)}`);
      }
    }
  }

  private invoke(method: string, params: unknown, timeoutMs?: number): Promise<RpcResponse> {
    const proc = this.proc;
    if (!proc) {
      return Promise.reject(new Error("pi-memory-rust 进程未启动"));
    }
    const id = this.nextId++;
    const timer = setTimeout(() => {
      const entry = this.pending.get(id);
      if (entry) {
        this.pending.delete(id);
        entry.reject(new Error(`调用超时：${method}`));
      }
      // 超时视为进程无响应，按崩溃处理以便重启
      proc.kill("SIGKILL");
      // 上面 kill 会触发 exit → 失败该请求，这里兜底避免悬挂
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
        reject(new Error("等待进程退出超时"));
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
 * 解析 pi-memory-server 二进制路径：
 * 1. 环境变量 PI_MEMORY_RUST_BINARY（显式指定）
 * 2. leaf 包（npm 安装场景，@jackice/pi-memory-rust-<platform>-<arch>）
 * 3. 包内 bin/（本地开发，pi install ./ts 时保留）
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
    // leaf 包未安装（本地开发/未发布），回退包内 bin
  }
  return new URL("../bin/pi-memory-server", import.meta.url).pathname;
}

