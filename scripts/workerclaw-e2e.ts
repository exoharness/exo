#!/usr/bin/env tsx

/**
 * Live E2E for WorkerClaw against a real exo binary + model provider.
 *
 * Verifies that a constrained turn calls `task_tree_init` then `complete_task`,
 * and that those tool calls show up in conversation events.
 *
 * Prerequisites:
 *   - Node + pnpm deps installed
 *   - ANTHROPIC_API_KEY and/or OPENAI_API_KEY in apps/exo/.env
 *
 * Run from the exo repo root:
 *
 *   pnpm e2e:workerclaw
 *
 * Options:
 *   --model <id>       Override model (default: claude-sonnet-4-6 or gpt-5.4)
 *   --root <path>      Use an explicit exo root
 *   --keep-root        Do not delete the temporary exo root
 *   --timeout-ms <ms>  Per-command timeout. Default: 180000
 */

import { spawnSync, type SpawnSyncReturns } from "node:child_process";
import { randomUUID } from "node:crypto";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  statSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

interface CliArgs {
  root: string | null;
  keepRoot: boolean;
  model: string | null;
  timeoutMs: number;
}

interface ProviderBinding {
  envName: string;
  secret: string;
  model: string;
  baseUrl?: string;
}

interface ConversationEventsResult {
  events: EventRecord[];
  cursor?: string | null;
}

interface EventRecord {
  id?: string;
  data?: {
    type?: string;
    tool_call_id?: string;
    request?: { function_name?: string };
    result?: unknown;
  };
}

const MODULE = "examples/workerclaw/harness.ts";
/** Matches agent-harness-e2e / docs. Override with --model. */
const DEFAULT_ANTHROPIC_MODEL = "claude-sonnet-4-6";
const DEFAULT_OPENAI_MODEL = "gpt-5.4";
const DEFAULT_TIMEOUT_MS = 180_000;

const E2E_USER_MESSAGE = `# Automated WorkerClaw E2E test

You MUST call exactly two tools in this order, then stop. Do not write any text response.

1. \`task_tree_init\` with rootRef "root" and nodes: one objective nodeRef "obj1", parentRef "root", depth 1, isLeaf false, title "Test objective", description "E2E", successCriteria "Init called", order 0

2. \`complete_task\` with summary "e2e-ok" and status "completed".`;

const E2E_COMPLETE_NUDGE = `# Continue WorkerClaw E2E test

Call \`complete_task\` now with summary "e2e-ok" and status "completed". Do not call any other tools. Do not write text.`;

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
loadDotEnv(join(repoRoot, ".env"), { override: true });

const args = parseArgs(process.argv.slice(2));
const runId = randomUUID().slice(0, 8);
const root = args.root ?? mkdtempSync(join(tmpdir(), "exo-workerclaw-e2e-"));
const exoBin = resolveExoBin();

await main();

async function main(): Promise<void> {
  const provider = resolveProvider();
  if (!provider) {
    fail(
      "no provider key set; export ANTHROPIC_API_KEY or OPENAI_API_KEY (or put them in .env)",
    );
  }

  log(`using exo root ${root}`);
  log(`exo bin=${exoBin}`);
  log(`provider secret=${provider.secret} model=${provider.model}`);
  log(`harness=${MODULE}`);
  log(
    `timeout=${args.timeoutMs}ms (Ctrl+C to abort; use --keep-root to inspect)`,
  );

  try {
    log("registering secret + model…");
    registerSecretAndModel(provider);
    const agentSlug = `e2e-workerclaw-${runId}`;
    const conversation = `job-${runId}`;

    log(`creating agent ${agentSlug} (local-process sandbox)…`);
    createAgent(agentSlug, provider);
    log(`creating conversation ${conversation}…`);
    runExo(["conversation", "create", agentSlug, "--slug", conversation]);

    log("sending constrained turn (live exo output below)…");
    runChat(agentSlug, conversation, E2E_USER_MESSAGE);

    let tools = collectToolNames(agentSlug, conversation);
    log(`tools after first turn: ${[...tools].join(", ") || "(none)"}`);

    if (tools.has("task_tree_init") && !tools.has("complete_task")) {
      log("task_tree_init seen but complete_task missing — sending nudge");
      runChat(agentSlug, conversation, E2E_COMPLETE_NUDGE);
      tools = collectToolNames(agentSlug, conversation);
      log(`tools after nudge: ${[...tools].join(", ") || "(none)"}`);
    }

    if (!tools.has("task_tree_init")) {
      throw new Error(
        `expected task_tree_init tool call; saw: ${[...tools].join(", ") || "none"}`,
      );
    }
    if (!tools.has("complete_task")) {
      throw new Error(
        `expected complete_task tool call; saw: ${[...tools].join(", ") || "none"}`,
      );
    }

    const completeResult = findToolResult(
      agentSlug,
      conversation,
      "complete_task",
    );
    const serialized = JSON.stringify(completeResult ?? null);
    if (!/e2e-ok|completed|ok\s*:\s*true|bridgeEvent/i.test(serialized)) {
      throw new Error(
        `complete_task result did not look successful:\n${serialized}`,
      );
    }

    log(
      "passed — task_tree_init + complete_task observed in conversation events",
    );
  } catch (error) {
    fail(errorMessage(error));
  } finally {
    if (!args.keepRoot) {
      rmSync(root, { recursive: true, force: true });
    } else {
      log(`kept exo root at ${root}`);
    }
  }
}

function resolveProvider(): ProviderBinding | null {
  const modelOverride =
    args.model ?? process.env.WORKERCLAW_E2E_MODEL?.trim() ?? null;

  if (process.env.ANTHROPIC_API_KEY?.trim()) {
    return {
      envName: "ANTHROPIC_API_KEY",
      secret: "anthropic",
      model: modelOverride ?? DEFAULT_ANTHROPIC_MODEL,
      // Do NOT set baseUrl to https://api.anthropic.com/v1 — the Anthropic SDK
      // already uses https://api.anthropic.com and appends /v1 itself.
    };
  }

  if (process.env.OPENAI_API_KEY?.trim()) {
    return {
      envName: "OPENAI_API_KEY",
      secret: "openai",
      model: modelOverride ?? DEFAULT_OPENAI_MODEL,
    };
  }

  return null;
}

function registerSecretAndModel(provider: ProviderBinding): void {
  runExo(["secret", "set", provider.secret, "--env", provider.envName]);
  const modelArgs = [
    "model",
    "register",
    provider.model,
    "--secret",
    provider.secret,
  ];
  if (provider.baseUrl) {
    modelArgs.push("--base-url", provider.baseUrl);
  }
  runExo(modelArgs);
}

function createAgent(slug: string, provider: ProviderBinding): void {
  // local-process avoids Docker/Apple-container/E2B startup hangs for a tools-only smoke.
  runExo([
    "--harness",
    "typescript",
    "--sandbox-backend",
    "local-process",
    "agent",
    "create",
    "WorkerClaw E2E",
    "--slug",
    slug,
    "--module",
    MODULE,
    "--model",
    provider.model,
    "--tool-creation",
    "disabled",
    "--networking",
    "enabled",
    "--sandbox-provider",
    "local-process",
    "--max-tool-round-trips",
    "6",
  ]);
}

function runChat(agent: string, conversation: string, prompt: string): void {
  const started = Date.now();
  runExo(["conversation", "send", agent, conversation, prompt], {
    timeoutMs: args.timeoutMs,
    inheritStdio: true,
  });
  log(`turn finished in ${((Date.now() - started) / 1000).toFixed(1)}s`);
}

function conversationEvents(
  agent: string,
  conversation: string,
  extraArgs: string[] = [],
): ConversationEventsResult {
  return parseJson<ConversationEventsResult>(
    runExo([
      "conversation",
      "events",
      agent,
      conversation,
      "--limit",
      "500",
      ...extraArgs,
    ]),
  );
}

function collectToolNames(agent: string, conversation: string): Set<string> {
  const names = new Set<string>();
  const events = conversationEvents(agent, conversation);
  for (const event of events.events) {
    const data = event.data;
    if (data?.type === "tool_requested") {
      const name = data.request?.function_name;
      if (name) names.add(name);
    }
  }
  return names;
}

function findToolResult(
  agent: string,
  conversation: string,
  toolName: string,
): unknown {
  const events = conversationEvents(agent, conversation);
  const callIds = new Set<string>();
  for (const event of events.events) {
    const data = event.data;
    if (
      data?.type === "tool_requested" &&
      data.request?.function_name === toolName &&
      data.tool_call_id
    ) {
      callIds.add(data.tool_call_id);
    }
  }
  for (const event of events.events) {
    const data = event.data;
    if (
      data?.type === "tool_result" &&
      data.tool_call_id &&
      callIds.has(data.tool_call_id)
    ) {
      return data.result;
    }
  }
  return null;
}

function resolveExoBin(): string {
  if (process.env.EXO_BIN) {
    return process.env.EXO_BIN;
  }
  // Prefer a freshly built debug binary; release can silently lag behind TS harness changes.
  const debug = join(repoRoot, "target/debug/exo");
  const release = join(repoRoot, "target/release/exo");
  const needsRebuild =
    !existsSync(debug) ||
    (existsSync(join(repoRoot, "typescript/harness/runner.ts")) &&
      existsSync(debug) &&
      isOlderThanSource(debug));
  if (needsRebuild) {
    log("building exo binary (cargo build -p exo)…");
    // inheritStdio is required: piping cargo output fills the OS pipe buffer and
    // deadlocks spawnSync (cargo blocks on write, parent waits for exit).
    run("cargo", ["build", "-p", "exo"], {
      timeoutMs: 10 * 60_000,
      inheritStdio: true,
    });
  }
  if (existsSync(debug)) {
    return debug;
  }
  if (existsSync(release)) {
    log(
      `warning: using possibly stale release binary at ${release}; set EXO_BIN or run cargo build -p exo`,
    );
    return release;
  }
  fail("exo binary not found; run `cargo build -p exo`");
}

function isOlderThanSource(binaryPath: string): boolean {
  try {
    const binaryMtime = statSync(binaryPath).mtimeMs;
    const markers = [
      join(repoRoot, "crates/cli/src/main.rs"),
      join(repoRoot, "crates/executor/src/typescript.rs"),
      join(repoRoot, "typescript/harness/runner.ts"),
    ];
    return markers.some((marker) => {
      try {
        return statSync(marker).mtimeMs > binaryMtime;
      } catch {
        return false;
      }
    });
  } catch {
    return true;
  }
}

function runExo(
  commandArgs: string[],
  options: { timeoutMs?: number; inheritStdio?: boolean } = {},
): string {
  return run(
    exoBin,
    ["--root", root, "--env-file-if-exists", ".env", ...commandArgs],
    options,
  );
}

function run(
  command: string,
  commandArgs: string[],
  options: { timeoutMs?: number; inheritStdio?: boolean } = {},
): string {
  const timeoutMs = options.timeoutMs ?? args.timeoutMs;
  const result: SpawnSyncReturns<string> = spawnSync(command, commandArgs, {
    cwd: repoRoot,
    env: process.env,
    encoding: "utf8",
    timeout: timeoutMs,
    stdio: options.inheritStdio ? "inherit" : ["ignore", "pipe", "pipe"],
  });
  if (result.error) {
    if (
      result.error &&
      "code" in result.error &&
      (result.error as NodeJS.ErrnoException).code === "ETIMEDOUT"
    ) {
      throw new Error(
        `${command} timed out after ${timeoutMs}ms. Re-run with --keep-root to inspect ${root}`,
      );
    }
    throw result.error;
  }
  if (result.status !== 0) {
    const rendered = options.inheritStdio
      ? ""
      : [result.stdout, result.stderr].filter(Boolean).join("\n").trim();
    throw new Error(
      `${command} ${commandArgs.join(" ")} failed with exit ${result.status}${rendered ? `\n${rendered}` : ""}`,
    );
  }
  return options.inheritStdio ? "" : (result.stdout ?? "").trim();
}

function parseArgs(rawArgs: string[]): CliArgs {
  const parsed: CliArgs = {
    root: null,
    keepRoot: false,
    model: null,
    timeoutMs: DEFAULT_TIMEOUT_MS,
  };
  for (let index = 0; index < rawArgs.length; index += 1) {
    const arg = rawArgs[index];
    if (arg === "--") {
      continue;
    } else if (arg === "--root") {
      parsed.root = readArgValue(rawArgs, ++index, arg);
    } else if (arg === "--keep-root") {
      parsed.keepRoot = true;
    } else if (arg === "--model") {
      parsed.model = readArgValue(rawArgs, ++index, arg);
    } else if (arg === "--timeout-ms") {
      parsed.timeoutMs = Number(readArgValue(rawArgs, ++index, arg));
      if (!Number.isFinite(parsed.timeoutMs) || parsed.timeoutMs <= 0) {
        fail("--timeout-ms must be a positive number");
      }
    } else if (arg === "--help" || arg === "-h") {
      printHelpAndExit();
    } else {
      fail(`unknown argument: ${arg}`);
    }
  }
  return parsed;
}

function readArgValue(rawArgs: string[], index: number, flag: string): string {
  const value = rawArgs[index];
  if (!value) {
    fail(`${flag} requires a value`);
  }
  return value;
}

function parseJson<T>(text: string): T {
  return JSON.parse(text) as T;
}

function loadDotEnv(path: string, options: { override: boolean }): void {
  if (!existsSync(path)) {
    return;
  }
  for (const line of readFileSync(path, "utf8").split(/\r?\n/u)) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) {
      continue;
    }
    const match = trimmed.match(
      /^(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/u,
    );
    if (!match) {
      continue;
    }
    const [, key, rawValue] = match;
    if (
      !key ||
      rawValue === undefined ||
      (!options.override && process.env[key] !== undefined)
    ) {
      continue;
    }
    process.env[key] = unquoteEnvValue(rawValue);
  }
}

function unquoteEnvValue(value: string): string {
  const trimmed = value.trim();
  if (
    (trimmed.startsWith('"') && trimmed.endsWith('"')) ||
    (trimmed.startsWith("'") && trimmed.endsWith("'"))
  ) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function log(message: string): void {
  console.log(`[workerclaw-e2e] ${message}`);
}

function fail(message: string): never {
  console.error(`[workerclaw-e2e] ${message}`);
  process.exit(1);
}

function printHelpAndExit(): never {
  console.log(`Usage: pnpm e2e:workerclaw [options]

Runs a live WorkerClaw turn against exoharness and asserts task_tree_init + complete_task.

Options:
  --model <id>        Model to register (default depends on provider key)
  --root <path>       Use an explicit exo root
  --keep-root         Do not delete the temporary exo root
  --timeout-ms <ms>   Per-command timeout. Default: ${DEFAULT_TIMEOUT_MS}
`);
  process.exit(0);
}
