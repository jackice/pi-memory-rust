/**
 * pi-memory-rust pi extension entry point (M3).
 *
 * Responsibilities: bind pi events, register the /memory command, manage the Rust resident process, LLM proxy.
 * LLM proxy: an optional LlmProvider (built on pi-ai's n() or an external service);
 * when present, it produces llmResult for retain/consolidate (Rust parses it or falls back to rules).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import type { Api, Model } from "@earendil-works/pi-ai";
import { createHash, randomUUID } from "node:crypto";
import { MemoryRustClient } from "./src/client.ts";
import { createPiAiLlmProvider, llmEnabled, type PiAiLlmContext } from "./src/llm.ts";

/** LLM provider interface (aligned with the pi-memory v0.1.2 LlmProvider). */
export interface LlmProvider {
  name: string;
  complete(prompt: string): Promise<string>;
}

interface UiLike {
  notify?: (message: string, level?: string) => void;
}

interface CtxLike {
  cwd?: string;
  sessionManager?: { getSessionId?: () => string };
  ui?: UiLike;
}

export interface PiMemoryRustOptions {
  /** LLM provider; when absent, retain/consolidate use the rule path (llm=off) */
  llm?: LlmProvider;
}

export default function registerPiMemoryRustExtension(
  pi: ExtensionAPI,
  options: PiMemoryRustOptions = {},
): void {
  const client = new MemoryRustClient();
  let prompts: { extract: string; consolidate: string } | null = null;
  const userPrompts = new Map<string, string>();
  const drainingTurns = new Map<string, Promise<void>>();
  const consolidatingSessions = new Map<string, Promise<void>>();
  const consolidationRetries = new Map<string, {
    attempts: number;
    lastAttemptAt: string;
    nextAttemptAt: number;
    lastError: string;
  }>();
  const pendingEnqueues = new Map<string, Set<Promise<void>>>();
  const costMetrics = {
    turns: 0,
    llmCalls: 0,
    llmFailures: 0,
    estimatedLlmTokens: 0,
    injectedMemories: 0,
    consolidationBudgetSkips: 0,
  };

  async function ensurePrompts(timeoutMs?: number): Promise<void> {
    if (prompts) return;
    prompts = await client.call("llmPrompts", {}, timeoutMs);
  }

  function trackPendingEnqueue(sessionId: string, projectScope: string, promise: Promise<void>): void {
    const key = `${projectScope}\0${sessionId}`;
    const pending = pendingEnqueues.get(key) ?? new Set<Promise<void>>();
    pending.add(promise);
    pendingEnqueues.set(key, pending);
    void promise.finally(() => {
      pending.delete(promise);
      if (pending.size === 0) pendingEnqueues.delete(key);
    });
  }

  async function waitForPendingEnqueues(sessionId: string, projectScope: string, budgetMs: number): Promise<void> {
    const pending = pendingEnqueues.get(`${projectScope}\0${sessionId}`);
    if (!pending?.size || budgetMs <= 0) return;
    let timer: NodeJS.Timeout | undefined;
    try {
      await Promise.race([
        Promise.allSettled([...pending]),
        new Promise<void>((resolve) => {
          timer = setTimeout(resolve, budgetMs);
          timer.unref();
        }),
      ]);
    } finally {
      if (timer) clearTimeout(timer);
    }
  }

  const SHUTDOWN_LLM_TIMEOUT_MS = 1000;
  const TURN_LLM_TIMEOUT_MS = 60_000;

  async function withTimeout<T>(promise: Promise<T>, timeoutMs: number, label: string): Promise<T> {
    return await new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`${label} timed out`)), timeoutMs);
      timer.unref();
      promise.then(
        (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        (error) => {
          clearTimeout(timer);
          reject(error);
        },
      );
    });
  }

  /**
   * Resolve the LLM provider:
   * 1. explicit injection (options.llm);
   * 2. default to pi-ai (the current session model); `PI_MEMORY_LLM=off` disables it (staying on the rule path);
   * 3. returns undefined without an available model (degrading to the rule path).
   */
  function resolveLlm(ctx: unknown): LlmProvider | undefined {
    if (options.llm) return options.llm;
    if (!llmEnabled()) return undefined;
    const c = ctx as { model?: Model<Api>; modelRegistry?: PiAiLlmContext["registry"] } | undefined;
    if (process.env.PI_MEMORY_LLM_DEBUG) {
      console.error(`[pi-memory-llm] model=${c?.model ? c.model.id : "undefined"} registry=${c?.modelRegistry ? "ok" : "undefined"}`);
    }
    if (!c?.model || !c?.modelRegistry) return undefined;
    return createPiAiLlmProvider({ model: c.model, registry: c.modelRegistry });
  }

  async function runLlm(prompt: string, ctx: unknown, timeoutMs = TURN_LLM_TIMEOUT_MS): Promise<{ result?: string; failed: boolean }> {
    const provider = resolveLlm(ctx);
    if (!provider) return { failed: false };
    costMetrics.llmCalls += 1;
    costMetrics.estimatedLlmTokens += estimateApproxTokens(prompt);
    try {
      const result = await withTimeout(provider.complete(prompt), timeoutMs, "LLM");
      costMetrics.estimatedLlmTokens += estimateApproxTokens(result);
      return { result, failed: false };
    } catch {
      costMetrics.llmFailures += 1;
      return { failed: true };
    }
  }

  async function runConsolidationLlm(
    prompt: string,
    ctx: unknown,
    timeoutMs: number,
  ): Promise<{ result?: string; failed: boolean }> {
    const configured = Number.parseInt(process.env.PI_MEMORY_CONSOLIDATE_MAX_INPUT_BYTES ?? "65536", 10);
    const maxBytes = Number.isFinite(configured) ? Math.max(1_024, Math.min(1_048_576, configured)) : 65_536;
    if (Buffer.byteLength(prompt, "utf8") > maxBytes) {
      costMetrics.consolidationBudgetSkips += 1;
      return { failed: true };
    }
    // The rule summary is the intentional degraded mode when no provider is available.
    if (!resolveLlm(ctx)) return { failed: true };
    return await runLlm(prompt, ctx, timeoutMs);
  }

  function drainPendingTurns(sessionId: string, projectScope: string, ctx: unknown, budgetMs = Number.POSITIVE_INFINITY, ruleOnly = false): Promise<void> {
    const key = `${projectScope}\0${sessionId}`;
    const running = drainingTurns.get(key);
    if (running) return running;
    const drain = (async () => {
      const deadline = Date.now() + budgetMs;
      while (Date.now() < deadline) {
        const owner = `${process.pid}:${sessionId}:${randomUUID()}`;
        const turn = await client.call<{
          sessionId: string; turnIndex: number; projectScope: string; text: string; promptMissing: boolean; attempts: number;
        } | null>("claimPendingTurn", { sessionId, projectScope, owner, leaseMs: 300_000 }, 5_000);
        if (!turn) return;
        let useRuleFallback = ruleOnly;
        if (!useRuleFallback) {
          try {
            await ensurePrompts();
          } catch (error) {
            useRuleFallback = true;
            notify(ctx as CtxLike | undefined, "retain", error);
          }
        }
        let heartbeatRunning = false;
        let leaseLost = false;
        const heartbeat = setInterval(() => {
          if (heartbeatRunning || leaseLost) return;
          heartbeatRunning = true;
          void client.call<{ renewed: boolean }>("heartbeatPendingTurn", {
            sessionId, projectScope, turnIndex: turn.turnIndex, owner, leaseMs: 300_000,
          }, 5_000).then(({ renewed }) => { if (!renewed) leaseLost = true; })
            .catch(() => { leaseLost = true; })
            .finally(() => { heartbeatRunning = false; });
        }, 60_000);
        let turnDone = false;
        try {
          let retained = false;
          let workingText = turn.text;
          for (let attempt = 0; attempt < 2 && !retained; attempt++) {
            const { result, failed } = useRuleFallback
              ? { failed: true as const }
              : await runLlm(`${prompts?.extract ?? ""}\n\n${workingText}`, ctx);
            if (leaseLost) throw new Error("pending turn lease lost");
            try {
              await client.call("retainTurnMemories", {
                turnIndex: turn.turnIndex,
                text: workingText,
                sessionId,
                projectScope,
                llmResult: result,
                llmFailed: failed,
                promptMissing: turn.promptMissing,
              }, 30_000);
              retained = true;
            } catch (error) {
              if (attempt === 0 && error instanceof Error && error.message.includes("SNAPSHOT_CHANGED")) {
                const latest = await client.call<{ text: string } | null>("getWorkingContent", {
                  sessionId,
                  turnIndex: turn.turnIndex,
                  projectScope,
                }, 5_000);
                if (!latest?.text) throw new Error("working snapshot changed but no current text is available");
                workingText = latest.text;
                continue;
              }
              throw error;
            }
          }
          if (!retained || leaseLost) throw new Error("pending turn could not be retained under its lease");
          const completed = await client.call<{ completed: boolean }>("completePendingTurn", {
            sessionId, projectScope, turnIndex: turn.turnIndex, owner,
          }, 5_000);
          if (!completed.completed) throw new Error("pending turn lease lost before completion");
          turnDone = true;
        } catch (error) {
          await client.call("failPendingTurn", {
            sessionId, projectScope, turnIndex: turn.turnIndex, owner,
            error: error instanceof Error ? error.message : String(error),
          }, 5_000).catch(() => {});
          notify(ctx as CtxLike | undefined, "retain", error);
        } finally {
          clearInterval(heartbeat);
        }
      }
    })().finally(() => drainingTurns.delete(key));
    drainingTurns.set(key, drain);
    return drain;
  }

  async function consolidateSession(sessionId: string, projectScope: string, ctx: unknown, timeoutMs = TURN_LLM_TIMEOUT_MS): Promise<void> {
    try { await ensurePrompts(); } catch { /* Rule summary remains available when prompt RPC is unavailable. */ }
    for (let round = 0; round < 3; round++) {
      type ConsolidationClaim = {
        claimToken: string;
        working: Array<{ id: string; content: string; sourceSha: string }>;
      };
      let claim: ConsolidationClaim;
      try {
        claim = await client.call<ConsolidationClaim>("claimConsolidation", {
          sessionId, projectScope, ttlMs: 180_000, limit: 100,
        }, 10_000);
      } catch (error) {
        if (!(error instanceof Error) || !error.message.includes("unknown method")) throw error;
        if (process.env.PI_MEMORY_LEGACY_CONSOLIDATION === "1") {
          await consolidateLegacy(sessionId, projectScope, ctx, timeoutMs);
          return;
        }
        throw new Error(
          "SNAPSHOT_REQUIRED: server does not support claim/commit consolidation; " +
          "upgrade the resident pi-memory server or explicitly set PI_MEMORY_LEGACY_CONSOLIDATION=1 for one-release compatibility",
        );
      }
      if (claim.working.length === 0) return;
      let heartbeatFailed = false;
      let heartbeatRunning = false;
      const heartbeat = setInterval(() => {
        if (heartbeatRunning || heartbeatFailed) return;
        heartbeatRunning = true;
        void client.call<{ renewed: boolean }>("heartbeatConsolidation", {
          projectScope, claimToken: claim.claimToken, ttlMs: 180_000,
        }, 5_000).then((result) => { if (!result.renewed) heartbeatFailed = true; })
          .catch(() => { heartbeatFailed = true; })
          .finally(() => { heartbeatRunning = false; });
      }, 60_000);
      let committed = false;
      let snapshotChanged = false;
      try {
        const source = claim.working.map((item) => item.content).join(";");
        const { result, failed } = await runConsolidationLlm(
          `${prompts?.consolidate ?? ""}\n\n${source}`,
          ctx,
          timeoutMs,
        );
        if (heartbeatFailed) throw new Error("consolidation lease lost");
        const commit = await client.call<{ committed: boolean; snapshotChanged: boolean }>("commitConsolidation", {
          projectScope, claimToken: claim.claimToken, summary: result, degraded: failed,
        }, 30_000);
        committed = commit.committed;
        snapshotChanged = commit.snapshotChanged;
        if (!committed && !snapshotChanged) throw new Error("consolidation claim was not committed");
      } finally {
        clearInterval(heartbeat);
        if (!committed) await client.call("releaseConsolidation", {
          projectScope, claimToken: claim.claimToken,
        }, 5_000).catch(() => {});
      }
      if (snapshotChanged) continue;
      const remaining = await client.call<{ count: number; dirty: boolean }>(
        "countUnconsolidated", { sessionId, projectScope }, 5_000,
      );
      if (remaining.count === 0 && !remaining.dirty) return;
      if (round < 2) await new Promise((resolve) => setTimeout(resolve, 100 + Math.floor(Math.random() * 300)));
    }
    throw new Error("consolidation did not converge after 3 rounds");
  }

  async function consolidateLegacy(
    sessionId: string,
    projectScope: string,
    ctx: unknown,
    timeoutMs: number,
  ): Promise<void> {
    let source: string;
    try {
      source = await client.call<string>("consolidateSource", { sessionId, projectScope }, 5_000);
    } catch (error) {
      if (error instanceof Error && error.message.includes("unknown method")) {
        throw new Error("legacy server does not expose consolidateSource; consolidation skipped");
      }
      throw error;
    }
    if (!source.trim()) return;
    const { result, failed } = await runConsolidationLlm(
      `${prompts?.consolidate ?? ""}\n\n${source}`,
      ctx,
      timeoutMs,
    );
    await client.call("consolidateSessionMemories", {
      sessionId, projectScope, llmResult: result, llmFailed: failed,
    }, 30_000);
  }

  function triggerConsolidation(
    sessionId: string,
    projectScope: string,
    ctx: unknown,
    force = false,
    timeoutMs = TURN_LLM_TIMEOUT_MS,
  ): Promise<void> {
    const key = `${projectScope}\0${sessionId}`;
    const running = consolidatingSessions.get(key);
    if (running) return running;
    const previous = consolidationRetries.get(key);
    if (!force && previous && Date.now() < previous.nextAttemptAt) return Promise.resolve();
    const attemptAt = new Date().toISOString();
    consolidationRetries.set(key, {
      attempts: previous?.attempts ?? 0,
      lastAttemptAt: attemptAt,
      nextAttemptAt: 0,
      lastError: previous?.lastError ?? "",
    });
    const promise = consolidateSession(sessionId, projectScope, ctx, timeoutMs)
      .then(() => {
        consolidationRetries.set(key, {
          attempts: 0,
          lastAttemptAt: attemptAt,
          nextAttemptAt: 0,
          lastError: "",
        });
      })
      .catch((error) => {
        const attempts = (previous?.attempts ?? 0) + 1;
        const delayMs = Math.min(5 * 60_000, 1_000 * (2 ** Math.min(attempts, 8)));
        const message = (error instanceof Error ? error.message : String(error))
          .replace(/\s+/g, " ")
          .slice(0, 300);
        consolidationRetries.set(key, {
          attempts,
          lastAttemptAt: attemptAt,
          nextAttemptAt: Date.now() + delayMs,
          lastError: message,
        });
        notify(ctx as CtxLike | undefined, "consolidate", error);
      })
      .finally(() => consolidatingSessions.delete(key));
    if (consolidationRetries.size > 256) {
      const oldest = consolidationRetries.keys().next().value;
      if (oldest) consolidationRetries.delete(oldest);
    }
    consolidatingSessions.set(key, promise);
    return promise;
  }

  async function maybeAutoConsolidate(sessionId: string, projectScope: string, ctx: unknown): Promise<void> {
    if (process.env.PI_MEMORY_AUTO_CONSOLIDATE === "0") return;
    try {
      const { count, dirty } = await client.call<{ count: number; dirty: boolean }>(
        "countUnconsolidated", { sessionId, projectScope }, 5_000,
      );
      const threshold = Math.max(1, Math.min(500, Number.parseInt(process.env.PI_MEMORY_CONSOLIDATE_EVERY ?? "20", 10) || 20));
      if (dirty || count >= threshold) await triggerConsolidation(sessionId, projectScope, ctx);
    } catch (error) {
      notify(ctx as CtxLike | undefined, "consolidate", error);
    }
  }

  pi.on("session_start", async (_event, ctx) => {
    try {
      await client.ensureStarted();
      const sessionId = resolveSessionId(ctx);
      const projectScope = resolveProjectScope(ctx);
      void drainPendingTurns(sessionId, projectScope, ctx)
        .then(() => maybeAutoConsolidate(sessionId, projectScope, ctx))
        .catch((error) => notify(ctx as CtxLike | undefined, "retain", error));
    } catch (error) {
      console.error(`[pi-memory-rust] recall unavailable: ${error instanceof Error ? error.message : String(error)}`);
    }
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    try {
      const sessionId = resolveSessionId(ctx);
      const projectScope = resolveProjectScope(ctx);
      await waitForPendingEnqueues(sessionId, projectScope, 1_000);
      let drainTimer: NodeJS.Timeout | undefined;
      await Promise.race([
        drainPendingTurns(sessionId, projectScope, ctx),
        new Promise<void>((resolve) => {
          drainTimer = setTimeout(resolve, 2_000);
          drainTimer.unref();
        }),
      ]);
      if (drainTimer) clearTimeout(drainTimer);
      if (process.env.PI_MEMORY_AUTO_CONSOLIDATE !== "0") await triggerConsolidation(
        sessionId, projectScope, ctx, false, SHUTDOWN_LLM_TIMEOUT_MS,
      );
    } catch (error) {
      notify(ctx as CtxLike | undefined, "consolidate", error);
    } finally {
      await client.shutdown().catch(() => {});
    }
  });

  pi.on("session_before_compact", async (_event, ctx) => {
    let timer: NodeJS.Timeout | undefined;
    try {
      const sessionId = resolveSessionId(ctx);
      const projectScope = resolveProjectScope(ctx);
      await waitForPendingEnqueues(sessionId, projectScope, 250);
      await Promise.race([
        drainPendingTurns(sessionId, projectScope, ctx, 1_250, true),
        new Promise<void>((resolve) => { timer = setTimeout(resolve, 1_250); timer.unref(); }),
      ]);
    } catch (error) {
      notify(ctx as CtxLike | undefined, "retain", error);
    } finally {
      if (timer) clearTimeout(timer);
    }
  });

  pi.on("session_compact", async (event, ctx) => {
    try {
      const summary = (event as { compactionEntry?: { summary?: unknown } }).compactionEntry?.summary;
      const summaryHash = createHash("sha256").update(typeof summary === "string" ? summary : "").digest("hex");
      await client.call("recordCompaction", {
        sessionId: resolveSessionId(ctx),
        projectScope: resolveProjectScope(ctx),
        reason: (event as { reason?: string }).reason ?? "unknown",
        summaryHash,
        firstKeptEntryId: (event as { compactionEntry?: { firstKeptEntryId?: string } }).compactionEntry?.firstKeptEntryId ?? "",
      });
      if (process.env.PI_MEMORY_AUTO_CONSOLIDATE !== "0") {
        void triggerConsolidation(resolveSessionId(ctx), resolveProjectScope(ctx), ctx);
      }
    } catch (error) {
      notify(ctx as CtxLike | undefined, "compaction", error);
    }
  });

  pi.on("session_compact_failed", (event) => {
    const detail = (event as { errorMessage?: string; reason?: string }).errorMessage ?? "compaction did not complete";
    if (process.env.PI_MEMORY_DEBUG) console.error(`[pi-memory-rust] compaction failed (${(event as { reason?: string }).reason ?? "unknown"}): ${detail}`);
  });

  pi.on("before_agent_start", async (event, ctx) => {
    // custom 消息触发（pi.sendMessage + triggerTurn）的运行没有用户 prompt，跳过召回
    const prompt = (event as { prompt?: unknown }).prompt;
    const sessionId = resolveSessionId(ctx);
    if (typeof prompt !== "string" || prompt.trim().length === 0) {
      userPrompts.delete(sessionId);
      return;
    }
    userPrompts.set(sessionId, prompt.trim());
    try {
      const injection = await client.call<string>("buildRecallInjection", {
        prompt,
        sessionId,
        projectScope: resolveProjectScope(ctx),
      });
      if (!injection) return;
      costMetrics.injectedMemories += injection.split("\n").filter((line) => line.startsWith("- [")).length;
      return {
        message: {
          customType: "pi-memory-rust-recall",
          content: injection,
          display: true,
        },
      };
    } catch (error) {
      notify(ctx as CtxLike | undefined, "recall", error);
    }
  });

  pi.on("turn_end", async (event, ctx) => {
    const sessionId = resolveSessionId(ctx);
    if ((event as { outcome?: string }).outcome === "aborted"
      || (event.message as { stopReason?: string }).stopReason === "aborted") {
      userPrompts.delete(sessionId);
      return;
    }
    costMetrics.turns += 1;
    const assistantText = extractText(event.message);
    const userPrompt = userPrompts.get(sessionId) ?? "";
    const promptMissing = userPrompt.length === 0;
    userPrompts.delete(sessionId);
    const text = [userPrompt, assistantText].filter(Boolean).join("\n\n");
    if (!text.trim()) return;
    const boundedText = truncateUtf8(text, 65_536);
    const projectScope = resolveProjectScope(ctx);
    const enqueue = (async () => {
      try {
        await client.call("enqueueTurn", { turnIndex: event.turnIndex, text: boundedText, sessionId, projectScope, promptMissing }, 2_000);
        void drainPendingTurns(sessionId, projectScope, ctx)
          .then(() => maybeAutoConsolidate(sessionId, projectScope, ctx))
          .catch((error) => notify(ctx as CtxLike | undefined, "retain", error));
      } catch (error) {
        if (!(error instanceof Error) || !error.message.includes("unknown method")) throw error;
        // Old resident servers predate the durable outbox; preserve compatibility.
        let promptAvailable = true;
        try {
          await ensurePrompts();
        } catch (promptError) {
          promptAvailable = false;
          notify(ctx as CtxLike | undefined, "retain", promptError);
        }
        const { result, failed } = promptAvailable
          ? await runLlm(`${prompts?.extract ?? ""}\n\n${boundedText}`, ctx)
          : { failed: true };
        await client.call("retainTurnMemories", {
          turnIndex: event.turnIndex, text: boundedText, sessionId, projectScope, llmResult: result, llmFailed: failed, promptMissing,
        });
      }
    })().catch((error) => notify(ctx as CtxLike | undefined, "retain", error));
    trackPendingEnqueue(sessionId, projectScope, enqueue);
  });

  pi.registerCommand("memory", {
    description: "pi-memory-rust memory maintenance (view/stats/diagnose/clear/purge/rebuild/rebuild-embeddings/consolidate/migrate/prepare-model/job)",
    handler: async (args, ctx) => {
      const [sub, ...rest] = (args ?? "").split(/\s+/).filter(Boolean);
      try {
        let result: string;
        if (sub === "job" && rest[0]) {
          result = await pollMaintenanceJob(client, rest[0]);
        } else if (sub === "prepare-model") {
          result = await runMaintenanceJob(client, "prepareModel", resolveProjectScope(ctx));
        } else if (sub === "consolidate") {
          await triggerConsolidation(resolveSessionId(ctx), resolveProjectScope(ctx), ctx, true);
          result = "consolidation requested";
        } else if (sub === "rebuild-embeddings") {
          result = await runMaintenanceJob(client, "rebuildEmbeddings", resolveProjectScope(ctx));
        } else if (sub === "rebuild") {
          result = await runMaintenanceJob(client, "rebuildFts", resolveProjectScope(ctx));
        } else if (sub === "clear" && rest[0] === "all" && rest[1] === "confirm") {
          result = await runMaintenanceJob(client, "clearAll", resolveProjectScope(ctx));
        } else if (sub === "clear" && rest.join(" ") === "all") {
          result = await client.call<string>("memoryCommand", {
            sub: "clear", target: "all", sessionId: resolveSessionId(ctx), projectScope: resolveProjectScope(ctx),
          });
        } else if (sub === "clear") {
          result = await runMaintenanceJob(
            client, "clearProject", resolveProjectScope(ctx), undefined, rest.join(" ") || resolveProjectScope(ctx),
          );
        } else if (sub === "purge") {
          result = await runMaintenanceJob(
            client, "purgeProject", resolveProjectScope(ctx), undefined, resolveProjectScope(ctx),
          );
        } else if (sub === "migrate" && rest[0]) {
          result = await runMaintenanceJob(client, "migrate", resolveProjectScope(ctx), rest.join(" "));
        } else {
          result = await client.call<string>("memoryCommand", {
            sub: sub ?? "view",
            target: rest.join(" "),
            sessionId: resolveSessionId(ctx),
            projectScope: resolveProjectScope(ctx),
          });
        }
        // Attach the real TS-side LLM status for diagnose (the Rust side cannot see pi's provider)
        if (sub === "diagnose") {
          const llm = resolveLlm(ctx);
          const turns = Math.max(1, costMetrics.turns);
          const consolidation = consolidationRetries.get(
            `${resolveProjectScope(ctx)}\0${resolveSessionId(ctx)}`,
          );
          let pendingConsolidation = "unknown";
          try {
            const pending = await client.call<{ count: number; dirty: boolean }>(
              "countUnconsolidated",
              { sessionId: resolveSessionId(ctx), projectScope: resolveProjectScope(ctx) },
              5_000,
            );
            pendingConsolidation = `${pending.count}${pending.dirty ? "+dirty" : ""}`;
          } catch {
            // Older resident servers do not expose countUnconsolidated.
          }
          result += ` llm=${llm ? llm.name : "off"}`
            + ` ops_metrics_turns=${costMetrics.turns}`
            + ` llm_calls_per_100_turns=${(costMetrics.llmCalls * 100 / turns).toFixed(1)}`
            + ` estimated_llm_tokens_per_100_turns=${Math.round(costMetrics.estimatedLlmTokens * 100 / turns)}`
            + ` llm_failure_pct=${(costMetrics.llmFailures * 100 / Math.max(1, costMetrics.llmCalls)).toFixed(1)}`
            + ` injected_memories_per_100_turns=${(costMetrics.injectedMemories * 100 / turns).toFixed(1)}`
            + ` consolidation_budget_skips=${costMetrics.consolidationBudgetSkips}`
            + ` consolidation_pending=${pendingConsolidation}`
            + ` consolidation_failures=${consolidation?.attempts ?? 0}`
            + ` consolidation_last_attempt=${consolidation?.lastAttemptAt ?? "never"}`
            + ` consolidation_next_attempt=${consolidation && consolidation.nextAttemptAt > 0
              ? new Date(consolidation.nextAttemptAt).toISOString()
              : "none"}`
            + ` consolidation_last_error=${JSON.stringify(consolidation?.lastError ?? "")}`;
        }
        ctx.ui.notify(result, "info");
      } catch (error) {
        notify(ctx, "command", error);
      }
    },
  });
}

interface MaintenanceJob {
  jobId: string;
  kind: string;
  projectScope: string;
  state: "queued" | "running" | "completed" | "failed";
  stage: string;
  completedItems: number;
  remainingItems: number | null;
  error: string | null;
  details?: unknown;
}

async function runMaintenanceJob(
  client: MemoryRustClient,
  kind: "prepareModel" | "migrate" | "rebuildEmbeddings" | "rebuildFts" | "clearProject" | "clearAll" | "purgeProject",
  projectScope: string,
  sourcePath?: string,
  targetScope?: string,
): Promise<string> {
  let job: MaintenanceJob;
  try {
    job = await client.call<MaintenanceJob>("startJob", {
      kind, projectScope, sourcePath, targetScope, confirmed: kind === "clearAll",
    }, 5_000);
  } catch (error) {
    if (!(error instanceof Error)) throw error;
    const unsupportedKind = error.message.includes("unsupported job kind")
      && (kind === "clearProject" || kind === "clearAll" || kind === "purgeProject");
    if (!error.message.includes("unknown method") && !unsupportedKind) throw error;
    // An already-running resident binary may predate job/status. Keep old servers usable;
    // only the new protocol can detach a long migration from its RPC connection.
    if (kind === "prepareModel") return await client.call<string>("prepareModel", {}, 5_000);
    if (kind === "migrate") {
      return JSON.stringify(await client.call("migrate", { sourcePath, projectScope }, 30 * 60_000));
    }
    if (kind === "rebuildFts") {
      return await client.call<string>("memoryCommand", {
        sub: "rebuild", target: "", sessionId: "", projectScope,
      }, 30 * 60_000);
    }
    if (kind === "clearProject" || kind === "clearAll") {
      return await client.call<string>("memoryCommand", {
        sub: "clear",
        target: kind === "clearAll" ? "all confirm" : (targetScope === projectScope ? "" : targetScope ?? ""),
        sessionId: "",
        projectScope,
      }, 30 * 60_000);
    }
    if (kind === "purgeProject") {
      return await client.call<string>("memoryCommand", {
        sub: "purge", target: "", sessionId: "", projectScope,
      }, 30 * 60_000);
    }
    let embedded = 0;
    let skippedEmpty = 0;
    let remaining = 0;
    for (let batch = 0; batch < 1_000; batch++) {
      const progress = await client.call<{ embedded: number; skippedEmpty: number; remaining: number }>(
        "backfillEmbeddings", { projectScope, maxItems: 32 }, 10_000,
      );
      embedded += progress.embedded;
      skippedEmpty = Math.max(skippedEmpty, progress.skippedEmpty);
      remaining = progress.remaining;
      if (remaining === 0 || progress.embedded === 0) break;
    }
    return `rebuild-embeddings: embedded=${embedded} skipped_empty=${skippedEmpty} remaining=${remaining}`;
  }
  return pollMaintenanceJob(client, job.jobId, job);
}

async function pollMaintenanceJob(
  client: MemoryRustClient,
  jobId: string,
  initial?: MaintenanceJob,
): Promise<string> {
  const deadline = Date.now() + 30 * 60_000;
  let status = initial ?? await client.call<MaintenanceJob>("jobStatus", { jobId }, 5_000);
  while (status.state === "queued" || status.state === "running") {
    if (Date.now() >= deadline) {
      throw new Error(`job ${jobId} is still running at ${status.stage}; use /memory job ${jobId} to check again`);
    }
    await new Promise((resolve) => setTimeout(resolve, 1_000));
    status = await client.call<MaintenanceJob>("jobStatus", { jobId }, 5_000);
  }
  if (status.state === "failed") {
    throw new Error(`${status.kind} failed at ${status.stage}: ${status.error ?? "unknown error"} (job ${jobId})`);
  }
  const details = status.details === undefined ? "" : `; details=${JSON.stringify(status.details)}`;
  return `${status.kind}: ${status.stage}; completed=${status.completedItems}; remaining=${status.remainingItems ?? "unknown"}${details}`;
}

function resolveProjectScope(ctx: unknown): string {
  return (ctx as { cwd?: string } | undefined)?.cwd ?? process.cwd();
}

const fallbackSessionId = `process-${process.pid}:${randomUUID()}`;
let warnedMissingSessionId = false;

function resolveSessionId(ctx: unknown): string {
  const sessionManager = (ctx as { sessionManager?: { getSessionId?: () => string } } | undefined)?.sessionManager;
  const managedSessionId = sessionManager?.getSessionId?.()?.trim();
  if (managedSessionId) return managedSessionId;
  const environmentSessionId = process.env.PI_SESSION_ID?.trim();
  if (environmentSessionId) return environmentSessionId;
  if (!warnedMissingSessionId) {
    warnedMissingSessionId = true;
    console.error("[pi-memory-rust] session id unavailable; using a process-scoped fallback id");
  }
  return fallbackSessionId;
}

function extractText(message: unknown): string {
  const content = (message as { content?: unknown } | undefined)?.content;
  if (!content) return "";
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  return content
    .filter((part): part is { type?: string; text?: string } => typeof part === "object" && part !== null)
    .filter((part) => part.type === "text")
    .map((part) => part.text ?? "")
    .join("\n");
}

function truncateUtf8(text: string, maxBytes: number): string {
  if (Buffer.byteLength(text, "utf8") <= maxBytes) return text;
  let bytes = 0;
  let end = 0;
  for (const character of text) {
    const width = Buffer.byteLength(character, "utf8");
    if (bytes + width > maxBytes) break;
    bytes += width;
    end += character.length;
  }
  return text.slice(0, end);
}

function estimateApproxTokens(text: string): number {
  let cjk = 0;
  let other = 0;
  for (const char of text) {
    const code = char.codePointAt(0) ?? 0;
    if ((code >= 0x3400 && code <= 0x4dbf)
      || (code >= 0x4e00 && code <= 0x9fff)
      || (code >= 0xf900 && code <= 0xfaff)
      || (code >= 0x20000 && code <= 0x3134f)) cjk += 1;
    else if (!/\s/.test(char)) other += 1;
  }
  return cjk + Math.ceil(other / 4);
}

function notify(ctx: { ui?: UiLike } | undefined, phase: string, error: unknown): void {
  const message = error instanceof Error ? error.message : String(error);
  const text = `pi-memory-rust ${phase} degraded: ${message}`;
  try {
    ctx?.ui?.notify?.(text, "warning");
  } catch {
    // 会话替换或 reload 后 ctx 已失效，ui getter 会抛错；降级提示失败不得影响收尾
    try {
      process.stderr.write(`${text}\n`);
    } catch {
      // stderr 不可写时丢弃提示
    }
  }
}

