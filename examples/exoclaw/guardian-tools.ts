import { spawn } from "node:child_process";

import type {
  HarnessToolRegistry,
  JsonObject,
  ToolDefinition,
  ToolInstance,
  ToolResult,
} from "@exo/harness";

const GUARDIAN_SCRIPT = new URL("./scripts/exoclaw-guardian", import.meta.url)
  .pathname;
const MAX_OUTPUT_CHARS = 20_000;
const DEFAULT_TIMEOUT_MS = 15 * 60 * 1000;

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
        "Ask the host-side Exoclaw guardian to build Exoclaw, inspect service status/logs, or restart the scheduler and adapter runners while preserving .exo state. Use this instead of manually killing host processes.",
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
          "Guardian action to run. restart_all restarts scheduler and adapters; set build=true to compile first.",
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

  const result = await runGuardian(commandArgs);
  return {
    ok: result.exitCode === 0,
    action: args.action,
    command: [GUARDIAN_SCRIPT, ...commandArgs],
    exitCode: result.exitCode,
    timedOut: result.timedOut,
    stdout: clampOutput(result.stdout),
    stderr: clampOutput(result.stderr),
  };
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
      cwd: new URL("../..", import.meta.url).pathname,
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

function clampOutput(output: string): string {
  if (output.length <= MAX_OUTPUT_CHARS) {
    return output;
  }
  return `${output.slice(0, MAX_OUTPUT_CHARS)}\n... truncated ${output.length - MAX_OUTPUT_CHARS} chars`;
}
