import {
  appendCustomEvent,
  assistantTextMessage,
  defineHarness,
  messagesEvent,
  messagesToTranscript,
  messageText,
  toolRequestedEvent,
  toolResultEvent,
  turnMetadata,
  toJsonValue,
  type EventData,
  type JsonObject,
  type JsonValue,
  type Message,
  type PendingToolCall,
  type TurnContext,
} from "@exo/harness";
import {
  traceExecutorTurn,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";
import {
  toOpencodeJson,
  type OpencodeWorkerEvent,
  type OpencodeWorkerRequest,
  type OpencodeWorkerRunResult,
} from "@exo/opencode/protocol";
import {
  appendAndTraceObservedToolEvents,
  materializePriorConversationMessages,
  resolveLlmBinding,
  sandboxCwd,
  WarmJsonlSandboxWorker,
  WarmResourceCache,
  type ResolvedLlmBinding,
} from "./shared";

interface OpencodeTraceState {
  finalText: string;
  llmPromptMessages: Message[];
  rawMessages: JsonValue[];
  startedAt: number;
  streamedText: string;
  ttftMs: number | null;
  sawTextDelta: boolean;
  promptMessages: Message[];
  runResult: OpencodeWorkerRunResult | null;
  observedToolCalls: Map<string, PendingToolCall>;
}

type OpencodeSandboxWorker = WarmJsonlSandboxWorker<
  OpencodeWorkerRequest,
  OpencodeWorkerEvent
>;

const opencodeWorkers = new WarmResourceCache<OpencodeSandboxWorker>();

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    await traceExecutorTurn(context, (turnParent) =>
      runOpencodeHarnessTurn(context, turnParent, modelBinding),
    );
  },
});

async function runOpencodeHarnessTurn(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): Promise<string | null> {
  const state: OpencodeTraceState = {
    finalText: "",
    llmPromptMessages: [],
    rawMessages: [],
    startedAt: Date.now(),
    streamedText: "",
    ttftMs: null,
    sawTextDelta: false,
    promptMessages: await materializeOpencodePromptMessages(context),
    runResult: null,
    observedToolCalls: new Map(),
  };
  const prompt = opencodePrompt(context, state.promptMessages);
  state.llmPromptMessages = [opencodePromptMessage(prompt)];

  await appendCustomEvent(
    context.exoharness.current.turn,
    "opencode_turn_started",
    {
      metadata: turnMetadata(context),
      model: modelBinding.model,
      cwd: sandboxCwd(context),
      hydrated_from: "exoharness_events",
      sandbox_command: opencodeSandboxCommand(context).join(" "),
    },
  );

  const result = await traceOpencodeRun(
    turnParent,
    context,
    state,
    prompt,
    modelBinding,
  );
  state.runResult = result;
  state.finalText = finalOpencodeText(state, result);
  await streamFinalTextSuffix(context, state);
  await appendOpencodeFinalEvents(context, state, result);
  if (result.status === "error") {
    throw new Error(result.result || "opencode run failed");
  }
  return null;
}

async function traceOpencodeRun(
  turnParent: TraceParent,
  context: TurnContext,
  state: OpencodeTraceState,
  prompt: string,
  modelBinding: ResolvedLlmBinding,
) {
  return tracedUnderParent(
    turnParent,
    async (span) => {
      try {
        const result = await runOpencodeSandboxWorker(
          context,
          turnParent,
          state,
          prompt,
          modelBinding,
        );
        state.runResult = result;
        state.finalText = finalOpencodeText(state, result);
        span.log({
          input: state.llmPromptMessages,
          output: opencodeTraceOutput(state, result),
          metrics: opencodeTraceMetrics(state, result),
        });
        return result;
      } catch (error) {
        const message = opencodeHarnessErrorMessage(error);
        span.log({
          input: state.llmPromptMessages,
          output: opencodeTraceOutput(state, state.runResult),
          metrics: opencodeTraceMetrics(state, state.runResult),
          error: message,
        });
        await appendCustomEvent(
          context.exoharness.current.turn,
          "opencode_run_failed",
          {
            metadata: turnMetadata(context),
            error: message,
          },
        );
        throw error;
      }
    },
    {
      name: `opencode:${modelBinding.model}`,
      type: "llm",
      spanAttributes: { purpose: "opencode_turn" },
      event: {
        input: state.llmPromptMessages,
        metadata: {
          ...turnMetadata(context),
          model: modelBinding.model,
          streamed: context.streaming,
        },
      },
    },
  );
}

async function runOpencodeSandboxWorker(
  context: TurnContext,
  turnParent: TraceParent,
  state: OpencodeTraceState,
  prompt: string,
  modelBinding: ResolvedLlmBinding,
): Promise<OpencodeWorkerRunResult> {
  const workerKey = opencodeWarmWorkerKey(context, modelBinding);
  const { resource: worker, reused } = await opencodeWorkers.get(
    workerKey,
    () => startOpencodeSandboxWorker(context, modelBinding),
  );
  await appendCustomEvent(
    context.exoharness.current.turn,
    "opencode_worker_ready",
    {
      metadata: turnMetadata(context),
      warm_worker_reused: reused,
    },
  );
  const request: OpencodeWorkerRequest = {
    prompt,
    model: modelBinding.model,
    cwd: sandboxCwd(context),
    apiKey: modelBinding.apiKey,
    baseUrl: modelBinding.baseUrl ?? undefined,
    provider: process.env.EXO_OPENCODE_PROVIDER,
    title: `exo:${context.exoharness.current.conversation.record.slug}`,
  };
  try {
    return await worker.request(request, async (event) => {
      await handleOpencodeWorkerEvent(context, turnParent, state, event);
      return event.type === "completed" ? event.result : undefined;
    });
  } catch (error) {
    await opencodeWorkers.delete(workerKey, (cachedWorker) =>
      cachedWorker.close(),
    );
    throw error;
  }
}

async function startOpencodeSandboxWorker(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): Promise<OpencodeSandboxWorker> {
  return new WarmJsonlSandboxWorker({
    name: "opencode sandbox worker",
    parseEvent: parseWorkerEvent,
    process: await context.startSandboxProcess({
      command: opencodeSandboxCommand(context),
      env: opencodeSandboxEnv(modelBinding),
    }),
  });
}

async function handleOpencodeWorkerEvent(
  context: TurnContext,
  turnParent: TraceParent,
  state: OpencodeTraceState,
  event: OpencodeWorkerEvent,
): Promise<void> {
  switch (event.type) {
    case "run_started":
      await appendCustomEvent(
        context.exoharness.current.turn,
        "opencode_run_started",
        {
          metadata: turnMetadata(context),
          session_id: event.sessionID,
        },
      );
      return;
    case "delta":
      await streamTextDelta(context, state, event.text);
      return;
    case "tool":
      await handleOpencodeTool(context, turnParent, state, event);
      return;
    case "message":
      state.rawMessages.push(event.message);
      return;
    case "completed":
      return;
    case "error":
      await appendCustomEvent(
        context.exoharness.current.turn,
        "opencode_worker_error",
        {
          metadata: turnMetadata(context),
          error: event.message,
          details: event.error,
        },
      );
      throw new Error(event.message);
  }
}

async function handleOpencodeTool(
  context: TurnContext,
  turnParent: TraceParent,
  state: OpencodeTraceState,
  event: Extract<OpencodeWorkerEvent, { type: "tool" }>,
): Promise<void> {
  const events: EventData[] =
    event.status === "running"
      ? [
          toolRequestedEvent({
            toolCallId: event.callId,
            request: {
              functionName: `opencode.${event.name}`,
              arguments: jsonObjectOrEmpty(event.args),
            },
          }),
        ]
      : [toolResultEvent(event.callId, toJsonValue(event.result ?? null))];
  await appendAndTraceObservedToolEvents(
    context,
    turnParent,
    events,
    state.observedToolCalls,
    "opencode_observed_tool",
  );
}

async function streamTextDelta(
  context: TurnContext,
  state: OpencodeTraceState,
  text: string,
): Promise<void> {
  if (!text) {
    return;
  }
  if (!state.sawTextDelta) {
    state.sawTextDelta = true;
    state.ttftMs = Date.now() - state.startedAt;
    if (context.streaming) {
      await context.stream.firstChunk(state.ttftMs);
    }
  }
  state.streamedText += text;
  if (context.streaming) {
    await context.stream.text(text);
  }
}

async function streamFinalTextSuffix(
  context: TurnContext,
  state: OpencodeTraceState,
): Promise<void> {
  if (!state.finalText || !context.streaming) {
    return;
  }
  if (!state.sawTextDelta) {
    await streamTextDelta(context, state, state.finalText);
    return;
  }
  if (state.finalText.startsWith(state.streamedText)) {
    const suffix = state.finalText.slice(state.streamedText.length);
    if (suffix) {
      await streamTextDelta(context, state, suffix);
    }
  }
}

async function appendOpencodeFinalEvents(
  context: TurnContext,
  state: OpencodeTraceState,
  result: OpencodeWorkerRunResult,
): Promise<void> {
  const events: EventData[] = [];
  if (state.finalText) {
    events.push(messagesEvent([assistantTextMessage(state.finalText)]));
  }
  if (events.length > 0) {
    await context.exoharness.current.turn.addEvents(events);
  }
  await flushOpencodeRawMessages(context, state);
  await appendCustomEvent(
    context.exoharness.current.turn,
    "opencode_run_completed",
    {
      metadata: turnMetadata(context),
      session_id: result.id,
      status: result.status,
      model: result.model ?? null,
      duration_ms: result.durationMs ?? null,
    },
  );
}

async function flushOpencodeRawMessages(
  context: TurnContext,
  state: OpencodeTraceState,
): Promise<void> {
  if (state.rawMessages.length === 0) {
    return;
  }
  await appendCustomEvent(
    context.exoharness.current.turn,
    "opencode_messages",
    {
      metadata: turnMetadata(context),
      messages: state.rawMessages,
    },
  );
}

function finalOpencodeText(
  state: OpencodeTraceState,
  result: OpencodeWorkerRunResult | null,
): string {
  if (result?.result && result.result.trim()) {
    return result.result;
  }
  return state.finalText;
}

function opencodePrompt(
  context: TurnContext,
  promptMessages: Message[],
): string {
  const transcript = messagesToTranscript(promptMessages);
  const currentInput = context.request.input.map(messageText).join("\n\n");
  const parts = [
    "You are opencode running inside exo's exoharness sandbox.",
    "Exoharness is the source of truth for durable conversation history. Treat the transcript below as the canonical prior state.",
    "You may inspect and modify files exposed through the sandbox filesystem. The sandbox mount and network policy are controlled by exo.",
    context.conversationConfig.shellProgram
      ? `Command execution, if available to opencode, runs inside the exoharness sandbox. Exo sandbox cwd: ${sandboxCwd(context)}.`
      : "Shell commands are disabled for this conversation.",
    transcript ? `Conversation so far:\n\n${transcript}` : null,
    `Current user input:\n\n${currentInput}`,
  ];
  return parts.filter(Boolean).join("\n\n");
}

function opencodePromptMessage(prompt: string): Message {
  return {
    role: "user",
    content: prompt,
  };
}

async function materializeOpencodePromptMessages(
  context: TurnContext,
): Promise<Message[]> {
  const priorMessages = await materializePriorConversationMessages(context);
  return [...context.agentConfig.instructions, ...priorMessages];
}

function opencodeTraceOutput(
  state: OpencodeTraceState,
  result: OpencodeWorkerRunResult | null,
): Record<string, unknown> {
  return {
    messages: state.finalText ? [assistantTextMessage(state.finalText)] : [],
    status: result?.status ?? "unknown",
    result: result?.result ?? null,
  };
}

function opencodeTraceMetrics(
  state: OpencodeTraceState,
  result: OpencodeWorkerRunResult | null,
): Record<string, number> {
  const metrics: Record<string, number> = {};
  if (state.ttftMs !== null) {
    metrics.time_to_first_token = state.ttftMs / 1000;
  }
  if (result?.durationMs !== undefined) {
    metrics.duration = result.durationMs / 1000;
  }
  return metrics;
}

function opencodeSandboxCommand(context: TurnContext): string[] {
  const shell = context.conversationConfig.shellProgram ?? "/bin/bash";
  const command =
    process.env.EXO_OPENCODE_SANDBOX_COMMAND ??
    "cd /opt/exo && node --import tsx typescript/opencode/sandbox-worker.ts";
  return [shell, "-lc", command];
}

function opencodeSandboxEnv(
  modelBinding: ResolvedLlmBinding,
): Record<string, string> {
  const env: Record<string, string> = {};
  env.HOME = process.env.EXO_OPENCODE_HOME ?? "/home/exo";
  for (const key of [
    "BRAINTRUST_API_KEY",
    "BRAINTRUST_APP_URL",
    "BRAINTRUST_API_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "EXO_OPENCODE_PROVIDER",
  ]) {
    const value = process.env[key];
    if (value) {
      env[key] = value;
    }
  }
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("OPENCODE_") && value) {
      env[key] = value;
    }
  }
  if (modelBinding.apiKey) {
    env.OPENCODE_API_KEY = modelBinding.apiKey;
  }
  return env;
}

function opencodeWarmWorkerKey(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): string {
  return JSON.stringify({
    agent_id: context.exoharness.current.agent.record.id,
    conversation_id: context.exoharness.current.conversation.record.id,
    model_binding: modelBinding.name,
    model: modelBinding.model,
    base_url: modelBinding.baseUrl ?? null,
    cwd: sandboxCwd(context),
    command: opencodeSandboxCommand(context),
  });
}

function parseWorkerEvent(line: string): OpencodeWorkerEvent {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed) || typeof parsed.type !== "string") {
    throw new Error(`invalid opencode sandbox worker event: ${line}`);
  }
  return parsed as OpencodeWorkerEvent;
}

function opencodeHarnessErrorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

function jsonObjectOrEmpty(value: unknown): JsonObject {
  return isRecord(value) ? (toOpencodeJson(value) as JsonObject) : {};
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
