import { spawn } from "node:child_process";
import { closeSync, mkdirSync, openSync } from "node:fs";
import { dirname, join } from "node:path";

import type {
  HarnessToolRegistry,
  JsonObject,
  ToolDefinition,
  ToolInstance,
  ToolResult,
} from "@exo/harness";

const GUARDIAN_SCRIPT = new URL(
  "./scripts/exoclaw-service-guardian",
  import.meta.url,
).pathname;
const ROOT_DIR = new URL("../..", import.meta.url).pathname;
const STATE_DIR = join(ROOT_DIR, ".exo");
const DEFERRED_LOG_PATH = join(
  STATE_DIR,
  "exoclaw-service-guardian-actions.log",
);
const MAX_OUTPUT_CHARS = 20_000;
const DEFAULT_TIMEOUT_MS = 15 * 60 * 1000;
const DEFERRED_RESTART_DELAY_SECONDS = 2;
const BUILD_MARKER = "EXO_BUILD_MARKER_2026_06_08_A";

type GuardianAction =
  | "status"
  | "build"
  | "start_services"
  | "stop_services"
  | "restart_services"
  | "restart_adapters"
  | "restart_scheduler"
  | "restart_all"
  | "logs";

type GuardianLogTarget = "scheduler" | "adapters" | "all";

const ACTION_TO_COMMAND: Record<GuardianAction, string> = {
  status: "status",
  build: "build",
  start_services: "start-services",
  stop_services: "stop-services",
  restart_services: "restart-services",
  restart_adapters: "restart-adapters",
  restart_scheduler: "restart-scheduler",
  restart_all: "restart-all",
  logs: "logs",
};

export function registerGuardianTools(registry: HarnessToolRegistry): void {
  registry.register(guardianActionTool());
}

function guardianActionTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "guardian_action",
      description:
        "Ask the host-side Exoclaw guardian to build Exoclaw, inspect service status/logs, or restart the scheduler and adapter runners while preserving .exo state. Builds request the control REPL wrapper to refresh its child process. Restart actions are deferred briefly so the current turn can finish before services are stopped. Use this instead of manually killing host processes.",
      parameters: guardianParameters(),
    },
    handler: {
      execute(args) {
        return executeGuardianAction(parseGuardianArguments(args));
      },
    },
  };
}

function guardianParameters(): ToolDefinition["parameters"] {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      action: {
        type: "string",
        enum: Object.keys(ACTION_TO_COMMAND),
        description:
          "Guardian action to run. restart_all restarts scheduler and adapters after a short deferred handoff; set build=true to compile first.",
      },
      build: {
        type: ["boolean", "null"],
        description:
          "For action restart_all, whether to build Exoclaw before restarting services.",
      },
      logTarget: {
        type: ["string", "null"],
        enum: ["scheduler", "adapters", "all", null],
        description:
          "For action logs, which log stream to show. Use null or all for both scheduler and adapters.",
      },
    },
    required: ["action", "build", "logTarget"],
  };
}

type GuardianArguments = {
  action: GuardianAction;
  build: boolean;
  logTarget: GuardianLogTarget;
};

function parseGuardianArguments(args: JsonObject): GuardianArguments {
  const action = args.action;
  if (typeof action !== "string" || !(action in ACTION_TO_COMMAND)) {
    throw new Error(
      "guardian_action action must be a supported guardian action",
    );
  }
  const build = args.build;
  if (build !== undefined && build !== null && typeof build !== "boolean") {
    throw new Error("guardian_action build must be boolean or null");
  }
  const logTarget = args.logTarget;
  if (
    logTarget !== undefined &&
    logTarget !== null &&
    logTarget !== "scheduler" &&
    logTarget !== "adapters" &&
    logTarget !== "all"
  ) {
    throw new Error(
      "guardian_action logTarget must be scheduler, adapters, all, or null",
    );
  }
  return {
    action: action as GuardianAction,
    build: build === true,
    logTarget: (logTarget ?? "all") as GuardianLogTarget,
  };
}

async function executeGuardianAction(
  args: GuardianArguments,
): Promise<ToolResult> {
  const command = ACTION_TO_COMMAND[args.action];
  const commandArgs = [command];
  if (args.action === "restart_all" && args.build) {
    commandArgs.push("--build");
  }
  if (args.action === "logs" && args.logTarget !== "all") {
    commandArgs.push(args.logTarget);
  }

  if (shouldDeferAction(args.action)) {
    const result = runGuardianDeferred(commandArgs);
    return {
      ok: true,
      action: args.action,
      deferred: true,
      pid: result.pid,
      delaySeconds: DEFERRED_RESTART_DELAY_SECONDS,
      command: result.command,
      logPath: result.logPath,
      stdout: `Scheduled guardian ${args.action} in ${DEFERRED_RESTART_DELAY_SECONDS}s. Follow ${result.logPath} for build/restart output, then call guardian_action logs or status after services come back.`,
      stderr: "",
    };
  }

  const result = await runGuardian(commandArgs);
  return {
    ok: result.exitCode === 0,
    action: args.action,
    buildMarker: BUILD_MARKER,
    command: [GUARDIAN_SCRIPT, ...commandArgs],
    exitCode: result.exitCode,
    timedOut: result.timedOut,
    stdout: clampOutput(result.stdout),
    stderr: clampOutput(result.stderr),
  };
}

function shouldDeferAction(action: GuardianAction): boolean {
  return (
    action === "restart_all" ||
    action === "restart_services" ||
    action === "restart_adapters" ||
    action === "restart_scheduler" ||
    action === "stop_services"
  );
}

type ProcessResult = {
  exitCode: number;
  timedOut: boolean;
  stdout: string;
  stderr: string;
};

function runGuardian(args: string[]): Promise<ProcessResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(GUARDIAN_SCRIPT, args, {
      cwd: ROOT_DIR,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
      setTimeout(() => child.kill("SIGKILL"), 1000).unref();
    }, DEFAULT_TIMEOUT_MS);

    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });
    child.on("error", (error) => {
      clearTimeout(timeout);
      reject(error);
    });
    child.on("close", (code) => {
      clearTimeout(timeout);
      resolve({
        exitCode: code ?? 1,
        timedOut,
        stdout,
        stderr,
      });
    });
  });
}

function runGuardianDeferred(args: string[]): {
  command: string[];
  logPath: string;
  pid: number | null;
} {
  mkdirSync(dirname(DEFERRED_LOG_PATH), { recursive: true });
  const logFd = openSync(DEFERRED_LOG_PATH, "a");
  const command = [GUARDIAN_SCRIPT, ...args];
  const child = spawn(
    "bash",
    [
      "-lc",
      'delay="$1"; shift; printf "\\n[%s] deferred guardian command:" "$(date -u +%Y-%m-%dT%H:%M:%SZ)"; for arg in "$@"; do printf " %q" "$arg"; done; printf "\\n"; sleep "$delay"; exec "$@"',
      "exoclaw-service-guardian-deferred",
      String(DEFERRED_RESTART_DELAY_SECONDS),
      ...command,
    ],
    {
      cwd: ROOT_DIR,
      detached: true,
      stdio: ["ignore", logFd, logFd],
    },
  );
  child.unref();
  closeSync(logFd);
  return {
    command,
    logPath: DEFERRED_LOG_PATH,
    pid: child.pid ?? null,
  };
}

function clampOutput(output: string): string {
  if (output.length <= MAX_OUTPUT_CHARS) {
    return output;
  }
  return `${output.slice(0, MAX_OUTPUT_CHARS)}\n... truncated ${output.length - MAX_OUTPUT_CHARS} chars`;
}
