import { EventEmitter } from "node:events";
import { PassThrough, Writable } from "node:stream";

import {
  query,
  type Options,
  type Query,
  type SDKMessage,
  type SDKResultMessage,
  type SDKUserMessage,
  type SpawnedProcess,
  type SpawnOptions,
} from "@anthropic-ai/claude-agent-sdk";
import {
  appendCustomEvent,
  assistantTextMessage,
  defineHarness,
  validateToolPolicies,
  materializeConversationMessages,
  messageText,
  messagesEvent,
  messagesToTranscript,
  systemTextMessage,
  toJsonValue,
  toJsonObject,
  turnMetadata,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type TurnContext,
} from "@exo/harness";
import {
  errorMessage,
  traceExecutorTurn,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";

import {
  appendEvents,
  appendAndTraceObservedToolEvents,
  asRecord,
  markFirstTextDelta,
  pickEnvFrom,
  projectAnthropicMessageToolEvents,
  resolveSandboxModel,
  sandboxCwd,
  type ResolvedModel,
} from "@exo/model-runtime/shared";

import { claudeToolName } from "../../typescript/harness/native-mcp";

const DEFAULT_CLAUDE_CODE_SANDBOX_EXECUTABLE = "/usr/local/bin/claude-code";
const CLAUDE_MAX_API_RETRIES = 2;
const CLAUDE_STDERR_PREVIEW_CHARS = 4_000;
const CLAUDE_STARTUP_TIMEOUT_MS = 60_000;

interface ClaudeTraceState {
  toolPoliciesValidated: boolean;
  startedAt: number;
  finalText: string;
  systemPrompt: string | null;
  promptMessages: Message[];
  rawMessages: JsonValue[];
  ttftMs: number | null;
  sawTextDelta: boolean;
  result: SDKResultMessage | null;
  finalMessageStored: boolean;
  observedToolCalls: Map<string, PendingToolCall>;
}

export default defineHarness({
  nativeToolApprovals: true,
  async runTurn(context) {
    const modelBinding = resolveSandboxModel(context);
    await traceExecutorTurn(context, (turnParent) =>
      runClaudeCodeTurn(context, turnParent, modelBinding),
    );
  },
});

async function runClaudeCodeTurn(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedModel,
): Promise<string | null> {
  const systemPrompt = claudeSystemPrompt(context);
  const state: ClaudeTraceState = {
    toolPoliciesValidated: false,
    startedAt: Date.now(),
    finalText: "",
    systemPrompt,
    promptMessages: await materializeClaudePromptMessages(
      context,
      systemPrompt,
    ),
    rawMessages: [],
    ttftMs: null,
    sawTextDelta: false,
    result: null,
    finalMessageStored: false,
    observedToolCalls: new Map(),
  };

  await appendCustomEvent(
    context.exoharness.current.turn,
    "claude_turn_started",
    {
      metadata: turnMetadata(context),
      model: modelBinding.model,
      hydrated_from: "exoharness_events",
    },
  );

  try {
    await traceClaudeLlmTurn(
      turnParent,
      context,
      state,
      modelBinding,
      async () => {
        await consumeClaudeQuery(
          query({
            prompt: claudePromptInput(claudePrompt(state.promptMessages)),
            options: claudeOptions(context, state, modelBinding),
          }),
          context,
          turnParent,
          state,
        );
      },
    );

    await appendClaudeFinalMessage(context, state, modelBinding.model);

    if (state.result?.type === "result" && state.result.is_error) {
      throw new Error(claudeResultError(state.result));
    }
  } finally {
    await flushClaudeRawMessages(context, state);
  }
  return null;
}

async function* claudePromptInput(
  prompt: string,
): AsyncIterable<SDKUserMessage> {
  yield {
    type: "user",
    message: {
      role: "user",
      content: prompt,
    },
    parent_tool_use_id: null,
  };
}

async function consumeClaudeQuery(
  claudeQuery: Query,
  context: TurnContext,
  turnParent: TraceParent,
  state: ClaudeTraceState,
): Promise<void> {
  let startupTimedOut = false;
  let sawSdkMessage = false;
  const startupTimer = setTimeout(() => {
    if (!sawSdkMessage) {
      startupTimedOut = true;
      claudeQuery.close();
    }
  }, CLAUDE_STARTUP_TIMEOUT_MS);
  startupTimer.unref?.();

  try {
    for await (const message of claudeQuery) {
      sawSdkMessage = true;
      clearTimeout(startupTimer);
      if (message.type === "system" && message.subtype === "init") {
        validateToolPolicies(
          context,
          message.tools.map(
            (name) =>
              claudeToolName(context.mcpServers, name) ?? `claude.${name}`,
          ),
        );
        state.toolPoliciesValidated = true;
      }
      await handleClaudeMessage(context, turnParent, state, message);
      const apiRetryError = claudeApiRetryLimitError(message);
      if (apiRetryError) {
        throw new Error(apiRetryError);
      }
      if (message.type === "result") {
        claudeQuery.close();
        break;
      }
    }
    if (startupTimedOut && !state.result && !state.finalText) {
      throw new Error(
        `Claude Code produced no SDK messages within ${CLAUDE_STARTUP_TIMEOUT_MS}ms; check claude_process_stderr events for process startup failures.`,
      );
    }
    if (!state.result) {
      throw new Error("Claude Code ended without a result");
    }
  } finally {
    clearTimeout(startupTimer);
    claudeQuery.close();
  }
}

async function traceClaudeLlmTurn(
  turnParent: TraceParent,
  context: TurnContext,
  state: ClaudeTraceState,
  modelBinding: ResolvedModel,
  run: () => Promise<void>,
): Promise<void> {
  await tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        await run();
        span.log({
          input: state.promptMessages,
          output: claudeTraceOutput(state),
          metrics: claudeUsageMetrics(state),
        });
      } catch (error) {
        span.log({
          input: state.promptMessages,
          output: claudeTraceOutput(state),
          metrics: claudeUsageMetrics(state),
          error: errorMessage(error),
        });
        throw error;
      }
    },
    {
      name: `claude-code:${modelBinding.model}`,
      type: "llm",
      spanAttributes: { purpose: "claude_code_llm_turn" },
      event: {
        input: state.promptMessages,
        metadata: {
          ...turnMetadata(context),
          runtime: "claude_agent_sdk",
          model: modelBinding.model,
          streamed: context.streaming,
        },
      },
    },
  );
}

function claudeOptions(
  context: TurnContext,
  state: ClaudeTraceState,
  modelBinding: ResolvedModel,
): Options {
  const options: Options = {
    model: modelBinding.model,
    cwd: sandboxCwd(context),
    persistSession: false,
    includePartialMessages: true,
    strictMcpConfig: true,
    disallowedTools: context.mcpServers.flatMap((server) =>
      server.disabledTools.map((tool) => `mcp__${server.name}__${tool}`),
    ),
    mcpServers: Object.fromEntries(
      context.mcpServers.map((server) => [
        server.name,
        {
          type: "http",
          url: server.url,
          ...(server.environmentVariable
            ? {
                headers: {
                  Authorization: "Bearer ${" + server.environmentVariable + "}",
                },
              }
            : {}),
        },
      ]),
    ),
    hooks: {
      PreToolUse: [
        {
          hooks: [
            async (input) => {
              if (input.hook_event_name !== "PreToolUse") return {};
              try {
                if (!state.toolPoliciesValidated) {
                  throw new Error(
                    "Claude Code has not reported its tool inventory",
                  );
                }
                const functionName = claudeToolName(
                  context.mcpServers,
                  input.tool_name,
                );
                if (!functionName) {
                  throw new Error(
                    `MCP tool is not enabled: ${input.tool_name}`,
                  );
                }
                await context.authorizeTool({
                  functionName,
                  arguments: toJsonObject(input.tool_input),
                });
                return {
                  hookSpecificOutput: {
                    hookEventName: "PreToolUse",
                    permissionDecision: "allow",
                  },
                };
              } catch (error) {
                return {
                  hookSpecificOutput: {
                    hookEventName: "PreToolUse",
                    permissionDecision: "deny",
                    permissionDecisionReason: errorMessage(error),
                  },
                };
              }
            },
          ],
        },
      ],
    },
    env: claudeSandboxBaseEnv(modelBinding),
    pathToClaudeCodeExecutable: claudeSandboxExecutable(),
    spawnClaudeCodeProcess: (options) =>
      new SandboxClaudeCodeProcess(context, options),
  };
  if (state.systemPrompt) {
    return { ...options, systemPrompt: state.systemPrompt };
  }
  return options;
}

async function handleClaudeMessage(
  context: TurnContext,
  turnParent: TraceParent,
  state: ClaudeTraceState,
  message: SDKMessage,
): Promise<void> {
  if (shouldStoreClaudeSdkMessage(message)) {
    state.rawMessages.push(toJsonValue(message));
  }

  if (message.type === "stream_event") {
    await handleClaudeStreamEvent(context, state, message.event);
    return;
  }

  if (message.type === "assistant") {
    await appendAndTraceObservedToolEvents(
      context,
      turnParent,
      projectAnthropicMessageToolEvents(message, {
        toolName: (name) =>
          claudeToolName(context.mcpServers, name) ?? `claude.${name}`,
      }),
      state.observedToolCalls,
      "claude_observed_tool",
    );
    const text = claudeAssistantText(message.message.content);
    if (text) {
      state.finalText = text;
    }
    return;
  }

  if (message.type === "user") {
    await appendAndTraceObservedToolEvents(
      context,
      turnParent,
      projectAnthropicMessageToolEvents(message, {
        toolName: (name) =>
          claudeToolName(context.mcpServers, name) ?? `claude.${name}`,
      }),
      state.observedToolCalls,
      "claude_observed_tool",
    );
    return;
  }

  if (message.type === "result") {
    state.result = message;
    await appendCustomEvent(context.exoharness.current.turn, "claude_result", {
      metadata: turnMetadata(context),
      result: toJsonValue(message),
    });
  }
}

function shouldStoreClaudeSdkMessage(message: SDKMessage): boolean {
  return message.type !== "stream_event";
}

async function appendClaudeFinalMessage(
  context: TurnContext,
  state: ClaudeTraceState,
  model: string,
): Promise<void> {
  if ((!state.finalText && !state.result) || state.finalMessageStored) {
    return;
  }
  state.finalMessageStored = true;
  const result = state.result;
  const usage = result?.usage;
  await appendEvents(context, [
    messagesEvent(
      state.finalText ? [assistantTextMessage(state.finalText)] : [],
      undefined,
      usage && result
        ? {
            model,
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            prompt_cached_tokens: usage.cache_read_input_tokens ?? 0,
            prompt_cache_creation_tokens:
              usage.cache_creation_input_tokens ?? 0,
            cost_usd: result.total_cost_usd,
          }
        : undefined,
    ),
  ]);
}

function claudeApiRetryLimitError(message: SDKMessage): string | null {
  const record = asRecord(message);
  if (record.type !== "system" || record.subtype !== "api_retry") {
    return null;
  }
  const attempt = record.attempt;
  if (typeof attempt !== "number" || attempt < CLAUDE_MAX_API_RETRIES) {
    return null;
  }
  const maxRetries =
    typeof record.max_retries === "number" ? record.max_retries : "unknown";
  const error = typeof record.error === "string" ? record.error : "unknown";
  const status =
    typeof record.error_status === "number" ? record.error_status : "none";
  return `Claude Code API request is still retrying after attempt ${attempt}/${maxRetries} (status: ${status}, error: ${error}); aborting instead of waiting for the full SDK retry backoff.`;
}

async function handleClaudeStreamEvent(
  context: TurnContext,
  state: ClaudeTraceState,
  event: unknown,
): Promise<void> {
  const record = asRecord(event);
  if (record.type !== "content_block_delta") {
    return;
  }
  const delta = asRecord(record.delta);
  if (delta.type !== "text_delta" || typeof delta.text !== "string") {
    return;
  }

  const ttftMs = markFirstTextDelta(state);
  if (ttftMs !== null) {
    if (context.streaming) {
      await context.stream.firstChunk(ttftMs);
    }
  }

  if (context.streaming) {
    await context.stream.text(delta.text);
  }
}

async function materializeClaudePromptMessages(
  context: TurnContext,
  systemPrompt: string | null,
): Promise<Message[]> {
  const messages = await materializeConversationMessages(
    context.exoharness.current.conversation,
  );
  const promptMessages = messages.filter(
    (message) => message.role !== "system" && message.role !== "developer",
  );
  if (!systemPrompt) {
    return promptMessages;
  }
  return [systemTextMessage(systemPrompt), ...promptMessages];
}

function claudePrompt(messages: Message[]): string {
  const conversational = messages.filter(
    (message) => message.role !== "system" && message.role !== "developer",
  );
  return messagesToTranscript(conversational);
}

function claudeSystemPrompt(context: TurnContext): string | null {
  const instructions = context.agentConfig.instructions
    .map(messageText)
    .filter(Boolean)
    .join("\n\n");
  return instructions || null;
}

function claudeAssistantText(content: unknown): string {
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((part) => {
      const record = asRecord(part);
      if (record.type === "text" && typeof record.text === "string") {
        return record.text;
      }
      return "";
    })
    .join("");
}

function claudeTraceOutput(state: ClaudeTraceState): Record<string, unknown> {
  return {
    messages: state.finalText ? [assistantTextMessage(state.finalText)] : [],
    tool_calls: [],
    status: state.result?.subtype ?? "completed",
  };
}

function claudeUsageMetrics(state: ClaudeTraceState): Record<string, number> {
  const metrics: Record<string, number> = {};
  const usage = state.result?.usage;
  if (usage) {
    metrics.prompt_tokens = usage.input_tokens;
    metrics.completion_tokens = usage.output_tokens;
    metrics.tokens = usage.input_tokens + usage.output_tokens;
    metrics.prompt_cached_tokens = usage.cache_read_input_tokens ?? 0;
    metrics.prompt_cache_creation_tokens =
      usage.cache_creation_input_tokens ?? 0;
  }
  if (state.result?.total_cost_usd !== undefined) {
    metrics.estimated_cost = state.result.total_cost_usd;
  }
  if (state.ttftMs !== null) {
    metrics.time_to_first_token = state.ttftMs / 1000;
  }
  return metrics;
}

async function flushClaudeRawMessages(
  context: TurnContext,
  state: ClaudeTraceState,
): Promise<void> {
  if (state.rawMessages.length === 0) {
    return;
  }
  await appendCustomEvent(
    context.exoharness.current.turn,
    "claude_sdk_messages",
    {
      metadata: turnMetadata(context),
      messages: state.rawMessages,
    },
  );
}

function claudeResultError(result: SDKResultMessage): string {
  if ("errors" in result && result.errors.length > 0) {
    return result.errors.join("\n");
  }
  const resultText = asRecord(result).result;
  if (typeof resultText === "string" && resultText.trim()) {
    return resultText;
  }
  return result.stop_reason ?? "claude code turn failed";
}

class SandboxClaudeCodeProcess extends EventEmitter implements SpawnedProcess {
  readonly stdin: Writable;
  readonly stdout = new PassThrough();
  killed = false;
  exitCode: number | null = null;
  private readonly pendingWrites: string[] = [];
  private sandboxProcess: Awaited<
    ReturnType<TurnContext["startSandboxProcess"]>
  > | null = null;
  private stdinEnded = false;

  constructor(
    private readonly turnContext: TurnContext,
    options: SpawnOptions,
  ) {
    super();
    this.stdin = new Writable({
      write: (chunk, _encoding, callback) => {
        const data = Buffer.isBuffer(chunk) ? chunk.toString() : String(chunk);
        this.writeStdin(data).then(() => callback(), callback);
      },
      final: (callback) => {
        this.closeStdin().then(() => callback(), callback);
      },
    });

    if (options.signal.aborted) {
      this.kill("SIGTERM");
      return;
    }
    options.signal.addEventListener(
      "abort",
      () => {
        this.kill("SIGTERM");
      },
      { once: true },
    );
    void this.start(this.turnContext, options);
  }

  kill(_signal: NodeJS.Signals): boolean {
    if (this.killed) {
      return true;
    }
    this.killed = true;
    if (this.sandboxProcess) {
      void this.sandboxProcess.close();
    }
    return true;
  }

  private async start(
    context: TurnContext,
    options: SpawnOptions,
  ): Promise<void> {
    try {
      await appendCustomEvent(
        context.exoharness.current.turn,
        "claude_process_starting",
        {
          metadata: turnMetadata(context),
          command: [options.command, ...options.args],
          cwd: options.cwd ?? null,
        },
      );
      const sandboxProcess = await context.startSandboxProcess({
        command: [options.command, ...options.args],
        env: claudeSandboxEnv(options.env),
      });
      await appendCustomEvent(
        context.exoharness.current.turn,
        "claude_process_started",
        {
          metadata: turnMetadata(context),
        },
      );
      if (this.killed) {
        await sandboxProcess.close();
        return;
      }
      void pumpSandboxReadable(sandboxProcess.stdout, this.stdout);
      void drainSandboxStderr(context, sandboxProcess.stderr);
      while (this.pendingWrites.length > 0) {
        const pendingWrite = this.pendingWrites.shift();
        if (pendingWrite !== undefined) {
          await sandboxProcess.writeStdin(pendingWrite);
        }
      }
      this.sandboxProcess = sandboxProcess;
      if (this.stdinEnded) {
        await sandboxProcess.closeStdin();
      }
      const exitCode = await sandboxProcess.wait();
      this.exitCode = exitCode;
      this.emit("exit", exitCode, null);
      this.stdout.end();
    } catch (error) {
      const normalized =
        error instanceof Error ? error : new Error(String(error));
      await appendCustomEvent(
        context.exoharness.current.turn,
        "claude_process_start_failed",
        {
          metadata: turnMetadata(context),
          error: normalized.message,
        },
      );
      this.emit("error", normalized);
      this.stdout.destroy(normalized);
    }
  }

  private async writeStdin(data: string): Promise<void> {
    if (this.killed || this.stdinEnded) {
      return;
    }
    if (!this.sandboxProcess) {
      this.pendingWrites.push(data);
      return;
    }
    await this.sandboxProcess.writeStdin(data);
  }

  private async closeStdin(): Promise<void> {
    if (this.stdinEnded) {
      return;
    }
    this.stdinEnded = true;
    if (!this.sandboxProcess) {
      return;
    }
    await this.sandboxProcess.closeStdin();
  }
}

async function pumpSandboxReadable(
  input: ReadableStream<string>,
  output: PassThrough,
): Promise<void> {
  try {
    const reader = input.getReader();
    try {
      for (;;) {
        const { value, done } = await reader.read();
        if (done) {
          break;
        }
        output.write(value);
      }
    } finally {
      reader.releaseLock();
    }
  } finally {
    output.end();
  }
}

async function drainSandboxStderr(
  context: TurnContext,
  input: ReadableStream<string>,
): Promise<void> {
  const reader = input.getReader();
  let chunkCount = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        return;
      }
      if (value && chunkCount < 3) {
        chunkCount += 1;
        await appendCustomEvent(
          context.exoharness.current.turn,
          "claude_process_stderr",
          {
            metadata: turnMetadata(context),
            chunk_index: chunkCount,
            text: value.slice(0, CLAUDE_STDERR_PREVIEW_CHARS),
            truncated: value.length > CLAUDE_STDERR_PREVIEW_CHARS,
          },
        );
      }
    }
  } finally {
    reader.releaseLock();
  }
}

function claudeSandboxExecutable(): string {
  return DEFAULT_CLAUDE_CODE_SANDBOX_EXECUTABLE;
}

function claudeSandboxBaseEnv(
  modelBinding: ResolvedModel,
): Record<string, string> {
  const env: Record<string, string> = {};
  if (modelBinding.baseUrl) {
    env.ANTHROPIC_BASE_URL = modelBinding.baseUrl;
  }
  return env;
}

function claudeSandboxEnv(
  env: Record<string, string | undefined>,
): Record<string, string> {
  const selected = pickEnvFrom(env, (key) => {
    return (
      key === "ANTHROPIC_BASE_URL" ||
      key === "CLAUDE_CONFIG_DIR" ||
      key === "TRACEPARENT" ||
      key === "TRACESTATE"
    );
  });
  selected.HOME ??= "/home/exo";
  selected.CLAUDE_CONFIG_DIR ??= "/home/exo/.claude";
  return selected;
}
