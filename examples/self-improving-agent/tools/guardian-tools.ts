import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  appendFileSync,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";

import type {
  HarnessToolRegistry,
  JsonObject,
  ToolExecutionContext,
  ToolInstance,
  ToolResult,
} from "@exo/harness";

const GUARDIAN_SCRIPT = new URL(
  "../scripts/exo-service-guardian",
  import.meta.url,
).pathname;
const DEFERRED_SCRIPT = new URL(
  "../scripts/deferred-rebuild-and-restart",
  import.meta.url,
).pathname;
const ROOT_DIR = new URL("../../..", import.meta.url).pathname;
const STATE_DIR = guardianStateDir(process.env, ROOT_DIR);
const DEFERRED_LOG_PATH = join(STATE_DIR, "exo-service-guardian-actions.log");
const UPDATE_DIR = join(STATE_DIR, "guardian-updates");
const EXO_BIN = join(ROOT_DIR, "target/debug/exo");
const ENV_FILE = join(ROOT_DIR, ".env");
const DEFERRED_RESTART_DELAY_SECONDS = 2;

export function guardianStateDir(
  env: NodeJS.ProcessEnv,
  repoRoot: string,
): string {
  return env.EXO_ROOT ?? join(repoRoot, ".exo");
}

export function registerGuardianTools(registry: HarnessToolRegistry): void {
  registry.register(rebuildAndRestartExoTool());
}

export function rebuildAndRestartExoTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "rebuild_and_restart_exo",
      description:
        "Validate and rebuild Exo, then restart its guardian-managed scheduler and adapter services. This narrow operation is asynchronous: it durably records an update, lets the current turn finish, and returns an update id. Pass reason as a short free-text note describing the change being activated so the update record is self-describing. On completion, the outcome is written to .exo/guardian-updates/<id>.json and recorded in this conversation's event log as rebuild_and_restart_exo (visible via list_conversation_events).",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          reason: {
            type: ["string", "null"],
            description:
              "Short free-text note describing why this rebuild was requested, for example the change being activated. Prefer a concrete description over null.",
          },
        },
        required: ["reason"],
      },
    },
    handler: {
      execute(args, execution) {
        return Promise.resolve(
          queueRebuildAndRestart(parseRebuildReason(args), execution),
        );
      },
    },
  };
}

export function parseRebuildReason(args: JsonObject): string | null {
  if (!("reason" in args) || args.reason === null) {
    return null;
  }
  if (typeof args.reason !== "string") {
    throw new Error("rebuild_and_restart_exo reason must be a string or null");
  }
  const reason = args.reason.trim();
  return reason.length > 0 ? reason : null;
}

function queueRebuildAndRestart(
  reason: string | null,
  execution: ToolExecutionContext,
): ToolResult {
  const { agent, conversation } = execution.context.exoharness.current;
  const updateId = randomUUID();
  const outcomePath = join(UPDATE_DIR, `${updateId}.json`);
  mkdirSync(UPDATE_DIR, { recursive: true });
  writeJsonAtomically(outcomePath, {
    updateId,
    operation: "rebuild_and_restart_exo",
    status: "queued",
    requestedAt: new Date().toISOString(),
    reason,
    agentId: agent.record.id,
    agentSlug: agent.record.slug,
    conversationId: conversation.record.id,
    conversationSlug: conversation.record.slug,
  });
  const missingScript = [DEFERRED_SCRIPT, GUARDIAN_SCRIPT].find(
    (script) => !existsSync(script),
  );
  if (missingScript) {
    // Fail closed: record a terminal outcome instead of leaving the update
    // queued forever when the deferred runner cannot be launched.
    writeFailedOutcome(outcomePath, {
      error: `deferred activation helper missing: ${missingScript}`,
    });
    throw new Error(
      `rebuild_and_restart_exo cannot start the deferred activation helper (missing: ${missingScript}); ` +
        `update ${updateId} recorded as failed in ${outcomePath}`,
    );
  }
  const result = runGuardianDeferredWithOutcome(
    ["restart-all", "--build"],
    updateId,
    outcomePath,
    reason,
  );
  return {
    ok: true,
    updateId,
    status: "queued",
    deferred: true,
    reason,
    agentId: agent.record.id,
    conversationId: conversation.record.id,
    pid: result.pid,
    delaySeconds: DEFERRED_RESTART_DELAY_SECONDS,
    outcomePath,
    logPath: result.logPath,
    command: result.command,
  };
}

function runGuardianDeferredWithOutcome(
  args: string[],
  updateId: string,
  outcomePath: string,
  reason: string | null,
): {
  command: string[];
  logPath: string;
  pid: number | null;
} {
  mkdirSync(dirname(DEFERRED_LOG_PATH), { recursive: true });
  const logFd = openSync(DEFERRED_LOG_PATH, "a");
  const command = [GUARDIAN_SCRIPT, ...args];
  const child = spawn(
    DEFERRED_SCRIPT,
    [
      String(DEFERRED_RESTART_DELAY_SECONDS),
      outcomePath,
      updateId,
      JSON.stringify(reason),
      EXO_BIN,
      ENV_FILE,
      STATE_DIR,
      ...command,
    ],
    {
      cwd: ROOT_DIR,
      detached: true,
      stdio: ["ignore", logFd, logFd],
    },
  );
  // A spawn failure (for example ENOENT on a missing helper script) is
  // otherwise silent: the deferred runner never executes, nothing surfaces,
  // and the durable update record stays "queued" forever. Give the attempt a
  // terminal, surfaced state instead.
  child.on("error", (error: NodeJS.ErrnoException) => {
    const message = `failed to spawn deferred activation helper (${error.code ?? "unknown"}): ${error.message}`;
    try {
      appendFileSync(DEFERRED_LOG_PATH, `${message}\n`);
    } catch {
      // The outcome file below is the durable record; the log is best-effort.
    }
    writeFailedOutcome(outcomePath, { error: message });
  });
  child.unref();
  closeSync(logFd);
  return {
    command,
    logPath: DEFERRED_LOG_PATH,
    pid: child.pid ?? null,
  };
}

export function writeFailedOutcome(
  outcomePath: string,
  extra: JsonObject,
): void {
  let record: JsonObject = {};
  try {
    record = JSON.parse(readFileSync(outcomePath, "utf8")) as JsonObject;
  } catch {
    // No queued record (or unreadable); still write a terminal one.
  }
  writeJsonAtomically(outcomePath, {
    ...record,
    status: "failed",
    failedAt: new Date().toISOString(),
    ...extra,
  });
}

function writeJsonAtomically(path: string, value: JsonObject): void {
  const temporaryPath = `${path}.tmp.${process.pid}`;
  writeFileSync(temporaryPath, `${JSON.stringify(value)}\n`, "utf8");
  renameSync(temporaryPath, path);
}
