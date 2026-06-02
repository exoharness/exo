import {
  assistantTextMessage,
  messageText,
  messagesEvent,
  stringifyValue,
  toJsonValue,
  toolRequestedEvent,
  toolResultEvent,
  type EventData,
  type JsonObject,
  type JsonValue,
  type Message,
  type PendingToolCall,
} from "@exo/harness";
import {
  CodexAppServer,
  type CodexNotification,
  type CodexProtocolLogEntry,
  type CodexServerRequest,
} from "@exo/codex/app-server";
import { responsesMessagesToLingua } from "@braintrust/lingua";

const CODEX_SHELL_TOOL = "codex.shell";
const CODEX_WEB_SEARCH_TOOL = "codex.web_search";
const EXO_SHELL_TOOL = "shell";
export const CODEX_EXO_SHELL_DYNAMIC_TOOL = "exo_shell";
const CODEX_PRIOR_MESSAGE_MAX_CHARS = 8_000;
const CODEX_PRIOR_TOOL_RESULT_MAX_CHARS = 4_000;
const CODEX_PRIOR_HISTORY_MAX_CHARS = 24_000;
export const CODEX_WARM_SESSION_EVENT = "codex_warm_session";

export interface CodexTurnRequest {
  input: Message[];
  priorMessages: Message[];
  model: string;
  modelProvider: string;
  developerInstructions?: string | null;
  cwd: string;
  sandboxRuntimeKey: JsonValue;
  warmSessionKey: string;
  dynamicTools?: JsonValue[];
  sandboxPolicy?: JsonValue;
  externalSandbox?: boolean;
  metadata?: JsonValue;
  streaming?: boolean;
}

export interface CodexTurnResult {
  threadId: string;
  finalText: string;
}

export interface CodexSandboxProcessRef {
  readonly sandboxId?: string;
  readonly sandboxProcessId?: string;
  readonly reused: boolean;
}

export interface CodexAppServerStartOptions {
  sessionKey: string;
  onProtocolMessage: (entry: CodexProtocolLogEntry) => Promise<void> | void;
  onServerRequest: (
    request: CodexServerRequest,
  ) => Promise<JsonValue | undefined> | JsonValue | undefined;
}

export interface CodexAppServerResource {
  server: CodexAppServer;
  process: CodexSandboxProcessRef;
}

export interface CodexAppServerProvider {
  start(options: CodexAppServerStartOptions): Promise<CodexAppServerResource>;
}

export interface CodexEventSink {
  append(data: EventData[]): Promise<void>;
  appendCustom(eventType: string, payload: JsonValue): Promise<void>;
}

export interface CodexStreamSink {
  firstChunk(ttftMs: number): Promise<void>;
  text(delta: string): Promise<void>;
  status(message: string): Promise<void>;
}

export interface CodexToolSink {
  call(toolCall: PendingToolCall): Promise<JsonValue>;
  observe?(toolCall: PendingToolCall, result: JsonValue): Promise<void>;
}

export interface CodexWarmSessionStore {
  latest(
    sessionKey: string,
    process: CodexSandboxProcessRef,
  ): Promise<CodexWarmSessionRecord | null>;
  record(
    sessionKey: string,
    process: CodexSandboxProcessRef,
    threadId: string,
  ): Promise<void>;
}

export interface CodexLlmTraceDetails {
  name: string;
  model: string;
  threadId: string;
  injectedResponseItems: number;
  input: Message[];
  metadata: JsonValue | null;
  streamed: boolean;
}

export interface CodexLlmTraceLog {
  input: Message[];
  output: Record<string, unknown>;
  metrics: Record<string, number>;
}

export interface CodexTraceSink {
  task<R>(name: string, input: unknown, run: () => Promise<R>): Promise<R>;
  llmTurn<R>(
    details: CodexLlmTraceDetails,
    run: () => Promise<R>,
    buildLog: () => CodexLlmTraceLog,
  ): Promise<R>;
}

export interface CodexTurnCapabilities {
  appServer: CodexAppServerProvider;
  eventSink: CodexEventSink;
  streamSink: CodexStreamSink;
  toolSink: CodexToolSink;
  warmSessionStore: CodexWarmSessionStore;
  trace?: CodexTraceSink;
}

interface CodexTokenUsage {
  inputTokens?: number;
  outputTokens?: number;
  totalTokens?: number;
  cachedInputTokens?: number;
  reasoningOutputTokens?: number;
}

interface CodexTurnTraceState {
  finalText: string;
  ttftMs: number | null;
  tokenUsage: CodexTokenUsage | null;
  promptMessages: Message[];
  startedAt: number;
  sawTextDelta: boolean;
}

interface CodexWarmTurnScope {
  request: CodexTurnRequest;
  capabilities: CodexTurnCapabilities;
  protocolLog: CodexProtocolEventBuffer;
}

interface PriorResponseItems {
  items: JsonValue[];
  sourceMessageCount: number;
  droppedMessageCount: number;
  truncatedMessageCount: number;
  textChars: number;
}

interface PriorResponseItemCandidate {
  item: JsonValue;
  textChars: number;
  truncated: boolean;
}

export interface CodexWarmSessionRecord {
  sessionKey: string;
  sandboxId: string | null;
  sandboxProcessId: string | null;
  threadId: string;
}

class WarmResourceCache<T> {
  private readonly entries = new Map<string, Promise<T>>();

  async get(
    key: string,
    create: () => Promise<T>,
  ): Promise<{ resource: T; reused: boolean }> {
    const existing = this.entries.get(key);
    if (existing) {
      return { resource: await existing, reused: true };
    }

    const created = create().catch((error: unknown) => {
      this.entries.delete(key);
      throw error;
    });
    this.entries.set(key, created);
    return { resource: await created, reused: false };
  }

  async delete(
    key: string,
    close?: (resource: T) => Promise<void> | void,
  ): Promise<void> {
    const existing = this.entries.get(key);
    this.entries.delete(key);
    if (!existing || !close) {
      return;
    }
    const resource = await existing;
    await close(resource);
  }
}

class CodexWarmSession {
  threadId: string | null;
  private current: CodexWarmTurnScope | null;

  private constructor(
    readonly server: CodexAppServer,
    readonly process: CodexSandboxProcessRef,
    initialScope: CodexWarmTurnScope,
    threadId: string | null,
  ) {
    this.current = initialScope;
    this.threadId = threadId;
  }

  static async start(
    scope: CodexWarmTurnScope,
    sessionKey: string,
  ): Promise<CodexWarmSession> {
    let session: CodexWarmSession | null = null;
    const pendingProtocol: CodexProtocolLogEntry[] = [];
    const resource = await scope.capabilities.appServer.start({
      sessionKey,
      onProtocolMessage: (entry: CodexProtocolLogEntry) => {
        if (session) {
          session.recordProtocol(entry);
        } else {
          pendingProtocol.push(entry);
        }
      },
      onServerRequest: (request: CodexServerRequest) =>
        session?.handleServerRequest(request),
    });
    const warmRecord = resource.process.reused
      ? await scope.capabilities.warmSessionStore.latest(
          sessionKey,
          resource.process,
        )
      : null;
    session = new CodexWarmSession(
      resource.server,
      resource.process,
      scope,
      resource.process.reused ? (warmRecord?.threadId ?? null) : null,
    );
    for (const entry of pendingProtocol) {
      session.recordProtocol(entry);
    }
    return session;
  }

  setTurnScope(scope: CodexWarmTurnScope): void {
    this.current = scope;
  }

  clearTurnScope(scope: CodexWarmTurnScope): void {
    if (this.current === scope) {
      this.current = null;
    }
  }

  close(): void {
    this.server.close();
  }

  private recordProtocol(entry: CodexProtocolLogEntry): void {
    this.current?.protocolLog.record(entry);
  }

  private handleServerRequest(
    request: CodexServerRequest,
  ): Promise<JsonValue | undefined> | JsonValue | undefined {
    const current = this.current;
    if (!current) {
      return undefined;
    }
    return handleCodexServerRequest(
      current.request,
      current.capabilities,
      request,
    );
  }
}

const codexSessions = new WarmResourceCache<CodexWarmSession>();

export async function runCodexTurn(
  request: CodexTurnRequest,
  capabilities: CodexTurnCapabilities,
): Promise<CodexTurnResult> {
  const protocolLog = new CodexProtocolEventBuffer(
    capabilities.eventSink,
    request.metadata ?? null,
  );
  const scope: CodexWarmTurnScope = { request, capabilities, protocolLog };
  const { resource: session, reused: appServerReused } = await traceCodexTask(
    capabilities,
    "codex_app_server_ready",
    {
      cwd: request.cwd,
      sandbox_process: true,
      sandbox_runtime: request.sandboxRuntimeKey,
      warm_session_key: request.warmSessionKey,
    },
    () =>
      codexSessions.get(request.warmSessionKey, () =>
        CodexWarmSession.start(scope, request.warmSessionKey),
      ),
  );
  session.setTurnScope(scope);

  const traceState: CodexTurnTraceState = {
    finalText: "",
    ttftMs: null,
    tokenUsage: null,
    promptMessages: [],
    startedAt: Date.now(),
    sawTextDelta: false,
  };

  try {
    const threadReused = session.threadId !== null;
    const threadId =
      session.threadId ??
      (await traceCodexTask(
        capabilities,
        "codex_thread_start",
        {
          runtime: "codex_app_server",
          model: request.model,
          cwd: request.cwd,
          external_sandbox: codexExternalSandbox(request),
        },
        () => startCodexThread(session.server, request),
      ));
    session.threadId = threadId;
    await capabilities.warmSessionStore.record(
      request.warmSessionKey,
      session.process,
      threadId,
    );

    const priorInjection = threadReused
      ? emptyPriorResponseItems()
      : messagesToResponseItems(request.priorMessages);
    const priorItems = priorInjection.items;
    if (priorItems.length > 0) {
      await traceCodexTask(
        capabilities,
        "codex_thread_inject_items",
        {
          thread_id: threadId,
          item_count: priorItems.length,
          source_message_count: priorInjection.sourceMessageCount,
          dropped_message_count: priorInjection.droppedMessageCount,
          truncated_message_count: priorInjection.truncatedMessageCount,
          text_chars: priorInjection.textChars,
        },
        () =>
          session.server.request("thread/inject_items", {
            threadId,
            items: priorItems,
          }),
      );
    }

    const turnInput = messagesToUserInput(request.input);
    const turnStart = await traceCodexTask(
      capabilities,
      "codex_turn_start",
      {
        thread_id: threadId,
        model: request.model,
        input: turnInput,
        external_sandbox: codexExternalSandbox(request),
      },
      () =>
        session.server.request<JsonObject>("turn/start", {
          threadId,
          input: turnInput,
          model: request.model,
          approvalPolicy: "on-request",
          sandboxPolicy:
            request.sandboxPolicy ?? codexNativeSandboxPolicy(request),
        }),
    );
    await capabilities.eventSink.appendCustom(
      "codex_turn_started",
      toJsonValue({
        metadata: request.metadata ?? null,
        codex_thread_id: threadId,
        codex_turn: turnStart.turn ?? null,
        hydrated_from: threadReused ? "warm_codex_thread" : "exoharness_events",
        injected_response_items: priorItems.length,
        warm_app_server_reused: appServerReused,
        warm_thread_reused: threadReused,
      }),
    );

    await traceCodexLlmTurn(
      capabilities,
      request,
      threadId,
      priorItems.length,
      traceState,
      async () => {
        let completed = false;
        const activeItems = new Set<string>();
        for await (const notification of session.server.events()) {
          const outcome = await handleCodexNotification(
            request,
            capabilities,
            notification,
            activeItems,
            traceState,
          );
          if (outcome === "completed") {
            completed = true;
            break;
          }
        }

        if (!completed) {
          throw new Error("codex app-server stopped before turn completed");
        }
      },
    );

    await protocolLog.flush();
    return {
      threadId,
      finalText: traceState.finalText,
    };
  } catch (error) {
    await codexSessions.delete(request.warmSessionKey, (warmSession) => {
      warmSession.close();
    });
    throw error;
  } finally {
    session.clearTurnScope(scope);
    await protocolLog.flush();
  }
}

async function traceCodexTask<R>(
  capabilities: CodexTurnCapabilities,
  name: string,
  input: unknown,
  run: () => Promise<R>,
): Promise<R> {
  return capabilities.trace?.task(name, input, run) ?? run();
}

async function traceCodexLlmTurn(
  capabilities: CodexTurnCapabilities,
  request: CodexTurnRequest,
  threadId: string,
  injectedResponseItems: number,
  traceState: CodexTurnTraceState,
  run: () => Promise<void>,
): Promise<void> {
  await (capabilities.trace?.llmTurn(
    {
      name: `codex:${request.model}`,
      model: request.model,
      threadId,
      injectedResponseItems,
      input: request.input,
      metadata: request.metadata ?? null,
      streamed: request.streaming ?? false,
    },
    run,
    () => ({
      input: codexLlmTraceInput(request, traceState),
      output: codexLlmTraceOutput(traceState),
      metrics: codexUsageMetrics(traceState),
    }),
  ) ?? run());
}

export function traceOutputPreview(value: unknown): unknown {
  if (typeof value === "string") {
    return value;
  }
  if (isRecord(value) && "resource" in value && "reused" in value) {
    return { reused: value.reused };
  }
  if (isRecord(value)) {
    return value;
  }
  return null;
}

function codexUsageMetrics(
  traceState: CodexTurnTraceState,
): Record<string, number> {
  const metrics: Record<string, number> = {};
  const usage = traceState.tokenUsage;
  if (usage?.inputTokens !== undefined) {
    metrics.prompt_tokens = usage.inputTokens;
  }
  if (usage?.outputTokens !== undefined) {
    metrics.completion_tokens = usage.outputTokens;
  }
  if (usage?.totalTokens !== undefined) {
    metrics.tokens = usage.totalTokens;
  }
  if (usage?.cachedInputTokens !== undefined) {
    metrics.prompt_cached_tokens = usage.cachedInputTokens;
  }
  if (usage?.reasoningOutputTokens !== undefined) {
    metrics.completion_reasoning_tokens = usage.reasoningOutputTokens;
  }
  if (traceState.ttftMs !== null) {
    metrics.time_to_first_token = traceState.ttftMs / 1000;
  }
  return metrics;
}

function codexLlmTraceInput(
  request: CodexTurnRequest,
  traceState: CodexTurnTraceState,
): Message[] {
  return traceState.promptMessages.length > 0
    ? traceState.promptMessages
    : request.input;
}

function codexLlmTraceOutput(
  traceState: CodexTurnTraceState,
): Record<string, unknown> {
  return {
    messages: traceState.finalText
      ? [assistantTextMessage(traceState.finalText)]
      : [],
    tool_calls: [],
    status: "completed",
  };
}

async function startCodexThread(
  codex: CodexAppServer,
  turnRequest: CodexTurnRequest,
): Promise<string> {
  const request: JsonObject = {
    model: turnRequest.model,
    modelProvider: turnRequest.modelProvider,
    cwd: turnRequest.cwd,
    approvalPolicy: "on-request",
    sandbox: "read-only",
    dynamicTools: turnRequest.dynamicTools ?? [],
    ephemeral: true,
    experimentalRawEvents: true,
    persistFullHistory: true,
  };
  if (turnRequest.developerInstructions) {
    request.developerInstructions = turnRequest.developerInstructions;
  }
  const response = await codex.request<JsonObject>("thread/start", request);
  const thread = response.thread;
  if (!isRecord(thread) || typeof thread.id !== "string") {
    throw new Error("codex thread/start response did not include thread.id");
  }
  return thread.id;
}

export function codexWarmSessionRecord(
  data: EventData,
): CodexWarmSessionRecord | null {
  if (data.type !== "custom" || data.event_type !== CODEX_WARM_SESSION_EVENT) {
    return null;
  }
  const payload = data.payload;
  if (!isRecord(payload)) {
    return null;
  }
  const sessionKey = payload.sessionKey;
  const threadId = payload.threadId;
  if (typeof sessionKey !== "string" || typeof threadId !== "string") {
    return null;
  }
  const sandboxId =
    typeof payload.sandboxId === "string" ? payload.sandboxId : null;
  const sandboxProcessId =
    typeof payload.sandboxProcessId === "string"
      ? payload.sandboxProcessId
      : null;
  return {
    sessionKey,
    sandboxId,
    sandboxProcessId,
    threadId,
  };
}

async function handleCodexServerRequest(
  turnRequest: CodexTurnRequest,
  capabilities: CodexTurnCapabilities,
  serverRequest: CodexServerRequest,
): Promise<JsonValue | undefined> {
  if (
    codexExternalSandbox(turnRequest) &&
    serverRequest.method === "item/commandExecution/requestApproval"
  ) {
    return { decision: "accept" };
  }
  if (serverRequest.method !== "item/tool/call") {
    return undefined;
  }
  return executeDynamicToolCall(capabilities, asRecord(serverRequest.params));
}

async function executeDynamicToolCall(
  capabilities: CodexTurnCapabilities,
  params: Record<string, unknown>,
): Promise<JsonValue> {
  const callId = stringOrNull(params.callId) ?? "dynamic-tool-call";
  const toolName = stringOrNull(params.tool);
  if (toolName !== CODEX_EXO_SHELL_DYNAMIC_TOOL) {
    return dynamicToolErrorResponse(`unsupported dynamic tool: ${toolName}`);
  }

  const args = asRecord(params.arguments);
  const command = stringOrNull(args.command);
  if (!command) {
    return dynamicToolErrorResponse("exo_shell requires a command string");
  }

  const toolCall: PendingToolCall = {
    toolCallId: callId,
    request: {
      functionName: EXO_SHELL_TOOL,
      arguments: objectArgs({ command }),
    },
  };

  try {
    const result = await capabilities.toolSink.call(toolCall);
    return dynamicToolResultResponse(shellToolResultText(result), {
      success: shellToolSucceeded(result),
    });
  } catch (error) {
    const message = errorMessage(error);
    return dynamicToolErrorResponse(message);
  }
}

async function handleCodexNotification(
  request: CodexTurnRequest,
  capabilities: CodexTurnCapabilities,
  notification: CodexNotification,
  activeItems: Set<string>,
  traceState: CodexTurnTraceState,
): Promise<"running" | "completed"> {
  updateTraceStateFromNotification(notification, traceState);
  switch (notification.method) {
    case "rawResponseItem/completed": {
      const params = asRecord(notification.params);
      const item = toJsonValue(params.item);
      await capabilities.eventSink.appendCustom(
        "codex_raw_response_item",
        toJsonValue({
          thread_id: params.threadId ?? null,
          turn_id: params.turnId ?? null,
          item,
        }),
      );
      return "running";
    }
    case "item/agentMessage/delta": {
      const params = asRecord(notification.params);
      if (typeof params.delta === "string") {
        const ttftMs = markFirstTextDelta(traceState);
        if (ttftMs !== null) {
          await capabilities.streamSink.firstChunk(ttftMs);
        }
        await capabilities.streamSink.text(params.delta);
      }
      return "running";
    }
    case "item/started": {
      const item = notificationItem(notification);
      const events = projectStartedItem(item, activeItems);
      await capabilities.eventSink.append(events);
      return "running";
    }
    case "item/completed": {
      const item = notificationItem(notification);
      const events = projectCompletedItem(item, activeItems);
      await capabilities.eventSink.append(events);
      const itemId = itemIdFromItem(item);
      const toolCall = itemId ? toolCallFromCodexItem(item, itemId) : null;
      if (toolCall) {
        await capabilities.toolSink.observe?.(
          toolCall,
          toolResultFromCodexItem(item),
        );
      }
      return "running";
    }
    case "turn/plan/updated":
      await capabilities.eventSink.appendCustom(
        "codex_plan_updated",
        toJsonValue(notification.params ?? null),
      );
      return "running";
    case "turn/diff/updated":
      await capabilities.eventSink.appendCustom(
        "codex_diff_updated",
        toJsonValue(notification.params ?? null),
      );
      return "running";
    case "thread/tokenUsage/updated":
      await capabilities.eventSink.appendCustom(
        "codex_token_usage",
        toJsonValue(notification.params ?? null),
      );
      return "running";
    case "turn/completed": {
      await capabilities.eventSink.appendCustom(
        "codex_turn_completed",
        toJsonValue(notification.params ?? null),
      );
      const params = asRecord(notification.params);
      const completedTurn = asRecord(params.turn);
      const status = completedTurn.status;
      if (status === "failed") {
        const message = codexTurnError(completedTurn);
        await streamCodexStatus(
          capabilities.streamSink,
          traceState,
          `error: ${message}`,
        );
        throw new Error(message);
      }
      return "completed";
    }
    case "error": {
      const params = asRecord(notification.params);
      const message = codexNotificationError(params);
      await capabilities.eventSink.appendCustom(
        params.willRetry === true ? "codex_retrying" : "codex_error",
        toJsonValue(notification.params ?? null),
      );
      if (params.willRetry === true) {
        await streamCodexStatus(
          capabilities.streamSink,
          traceState,
          `retrying: ${message}`,
        );
        return "running";
      }
      await streamCodexStatus(
        capabilities.streamSink,
        traceState,
        `error: ${message}`,
      );
      throw new Error(message);
    }
    default:
      return "running";
  }
}

function projectStartedItem(
  item: Record<string, unknown>,
  activeItems: Set<string>,
): EventData[] {
  const itemId = itemIdFromItem(item);
  if (!itemId) {
    return [];
  }
  const toolCall = toolCallFromCodexItem(item, itemId);
  if (!toolCall) {
    return [];
  }
  activeItems.add(itemId);
  return [toolRequestedEvent(toolCall)];
}

function projectCompletedItem(
  item: Record<string, unknown>,
  activeItems: Set<string>,
): EventData[] {
  const type = item.type;
  if (type === "agentMessage" && typeof item.text === "string") {
    return [messagesEvent([assistantTextMessage(item.text)])];
  }

  const itemId = itemIdFromItem(item);
  if (!itemId) {
    return [customItemEvent("codex_item_completed", item)];
  }

  const events: EventData[] = [];
  const toolCall = toolCallFromCodexItem(item, itemId);
  if (toolCall && !activeItems.has(itemId)) {
    events.push(toolRequestedEvent(toolCall));
  }
  if (toolCall) {
    events.push(toolResultEvent(itemId, toolResultFromCodexItem(item)));
    activeItems.delete(itemId);
    return events;
  }

  if (type === "fileChange") {
    return [customItemEvent("codex_file_change", item)];
  }
  if (type === "reasoning") {
    return [customItemEvent("codex_reasoning", item)];
  }
  if (type === "todoList") {
    return [customItemEvent("codex_todo_list", item)];
  }
  return [customItemEvent("codex_item_completed", item)];
}

function toolCallFromCodexItem(
  item: Record<string, unknown>,
  itemId: string,
): PendingToolCall | null {
  if (item.type === "commandExecution" && typeof item.command === "string") {
    return {
      toolCallId: itemId,
      request: {
        functionName: CODEX_SHELL_TOOL,
        arguments: objectArgs({
          command: item.command,
          cwd: stringOrNull(item.cwd),
        }),
      },
    };
  }

  if (item.type === "mcpToolCall") {
    return {
      toolCallId: itemId,
      request: {
        functionName: `codex.mcp.${String(item.server ?? "unknown")}.${String(item.tool ?? "unknown")}`,
        arguments: objectArgs({
          server: stringOrNull(item.server),
          tool: stringOrNull(item.tool),
          arguments: toJsonValue(item.arguments ?? null),
        }),
      },
    };
  }

  if (item.type === "webSearch") {
    return {
      toolCallId: itemId,
      request: {
        functionName: CODEX_WEB_SEARCH_TOOL,
        arguments: objectArgs({
          query: stringOrNull(item.query),
          action: toJsonValue(item.action ?? null),
        }),
      },
    };
  }

  return null;
}

function toolResultFromCodexItem(item: Record<string, unknown>): JsonValue {
  if (item.type === "commandExecution") {
    return toJsonValue({
      status: item.status ?? null,
      exit_code: item.exitCode ?? null,
      output: item.aggregatedOutput ?? null,
      duration_ms: item.durationMs ?? null,
    });
  }
  if (item.type === "mcpToolCall") {
    return toJsonValue({
      status: item.status ?? null,
      result: item.result ?? null,
      error: item.error ?? null,
    });
  }
  return toJsonValue({
    status: item.status ?? null,
    result: item,
  });
}

function emptyPriorResponseItems(): PriorResponseItems {
  return {
    items: [],
    sourceMessageCount: 0,
    droppedMessageCount: 0,
    truncatedMessageCount: 0,
    textChars: 0,
  };
}

function messagesToResponseItems(messages: Message[]): PriorResponseItems {
  const candidates = messages
    .filter(
      (message) => message.role !== "system" && message.role !== "developer",
    )
    .map(priorMessageToResponseItemCandidate);
  const selected: PriorResponseItemCandidate[] = [];
  let textChars = 0;
  let droppedMessageCount = 0;

  for (let index = candidates.length - 1; index >= 0; index -= 1) {
    const candidate = candidates[index];
    if (
      selected.length > 0 &&
      textChars + candidate.textChars > CODEX_PRIOR_HISTORY_MAX_CHARS
    ) {
      droppedMessageCount += 1;
      continue;
    }
    selected.push(candidate);
    textChars += candidate.textChars;
  }

  selected.reverse();
  return {
    items: selected.map((candidate) => candidate.item),
    sourceMessageCount: candidates.length,
    droppedMessageCount,
    truncatedMessageCount: candidates.filter((candidate) => candidate.truncated)
      .length,
    textChars,
  };
}

function priorMessageToResponseItemCandidate(
  message: Message,
): PriorResponseItemCandidate {
  const { text, truncated } = truncatePriorMessageText(
    messageText(message),
    priorMessageMaxChars(message),
  );
  if (message.role === "assistant") {
    return {
      item: toJsonValue({
        type: "message",
        role: "assistant",
        content: [{ type: "output_text", text }],
      }),
      textChars: text.length,
      truncated,
    };
  }
  return {
    item: toJsonValue({
      type: "message",
      role: "user",
      content: [{ type: "input_text", text }],
    }),
    textChars: text.length,
    truncated,
  };
}

function priorMessageMaxChars(message: Message): number {
  return message.role === "tool"
    ? CODEX_PRIOR_TOOL_RESULT_MAX_CHARS
    : CODEX_PRIOR_MESSAGE_MAX_CHARS;
}

function truncatePriorMessageText(
  text: string,
  maxChars: number,
): { text: string; truncated: boolean } {
  if (text.length <= maxChars) {
    return { text, truncated: false };
  }
  const omittedChars = text.length - maxChars;
  const suffix = `\n\n[truncated ${omittedChars} characters from prior conversation history]`;
  return {
    text: `${text.slice(0, Math.max(0, maxChars - suffix.length))}${suffix}`,
    truncated: true,
  };
}

function messagesToUserInput(messages: Message[]): JsonValue[] {
  const text = messages
    .filter((message) => message.role === "user")
    .map(messageText)
    .join("\n\n");
  return [
    {
      type: "text",
      text: text || messages.map(messageText).join("\n\n"),
      text_elements: [],
    },
  ];
}

function codexNativeSandboxPolicy(request: CodexTurnRequest): JsonValue {
  if (codexExternalSandbox(request)) {
    return {
      type: "externalSandbox",
      networkAccess: "restricted",
    };
  }
  return {
    type: "readOnly",
    networkAccess: false,
  };
}

function codexExternalSandbox(request: CodexTurnRequest): boolean {
  return request.externalSandbox ?? true;
}

function dynamicToolResultResponse(
  text: string,
  options: { success: boolean },
): JsonValue {
  return toJsonValue({
    contentItems: [{ type: "inputText", text }],
    success: options.success,
  });
}

function dynamicToolErrorResponse(message: string): JsonValue {
  return dynamicToolResultResponse(`Error: ${message}`, { success: false });
}

function notificationItem(
  notification: CodexNotification,
): Record<string, unknown> {
  const params = asRecord(notification.params);
  return asRecord(params.item);
}

function itemIdFromItem(item: Record<string, unknown>): string | null {
  return typeof item.id === "string" ? item.id : null;
}

function customItemEvent(
  eventType: string,
  item: Record<string, unknown>,
): EventData {
  return {
    type: "custom",
    event_type: eventType,
    payload: toJsonValue(item),
  };
}

function codexTurnError(turn: Record<string, unknown>): string {
  const error = turn.error;
  if (isRecord(error) && typeof error.message === "string") {
    const details =
      typeof error.additionalDetails === "string"
        ? ` (${error.additionalDetails})`
        : "";
    return `${error.message}${details}`;
  }
  return "codex turn failed";
}

function codexNotificationError(params: Record<string, unknown>): string {
  const error = asRecord(params.error);
  const message =
    typeof error.message === "string" ? error.message : stringifyValue(params);
  const details =
    typeof error.additionalDetails === "string"
      ? ` (${error.additionalDetails})`
      : "";
  return `${message}${details}`;
}

async function streamCodexStatus(
  streamSink: CodexStreamSink,
  traceState: CodexTurnTraceState,
  message: string,
): Promise<void> {
  const ttftMs = markFirstTextDelta(traceState);
  if (ttftMs !== null) {
    await streamSink.firstChunk(ttftMs);
  }
  await streamSink.status(message);
}

function updateTraceStateFromNotification(
  notification: CodexNotification,
  traceState: CodexTurnTraceState,
): void {
  if (notification.method === "rawResponseItem/completed") {
    const params = asRecord(notification.params);
    const item = asRecord(params.item);
    const message = rawCodexPromptMessage(item);
    if (message) {
      traceState.promptMessages.push(message);
    }
    return;
  }
  if (notification.method === "item/agentMessage/delta") {
    const params = asRecord(notification.params);
    if (typeof params.delta === "string") {
      traceState.finalText += params.delta;
    }
    return;
  }
  if (notification.method === "item/completed") {
    const item = notificationItem(notification);
    if (item.type === "agentMessage" && typeof item.text === "string") {
      traceState.finalText = item.text;
    }
    return;
  }
  if (notification.method === "thread/tokenUsage/updated") {
    const params = asRecord(notification.params);
    const tokenUsage = asRecord(params.tokenUsage);
    const last = asRecord(tokenUsage.last);
    traceState.tokenUsage = {
      inputTokens: numberField(last.inputTokens),
      outputTokens: numberField(last.outputTokens),
      totalTokens: numberField(last.totalTokens),
      cachedInputTokens: numberField(last.cachedInputTokens),
      reasoningOutputTokens: numberField(last.reasoningOutputTokens),
    };
  }
}

function rawCodexPromptMessage(item: Record<string, unknown>): Message | null {
  if (item.type !== "message") {
    return null;
  }
  if (!isRawCodexPromptRole(item.role)) {
    return null;
  }
  const messages = responsesMessagesToLingua([item]) as Message[];
  return messages[0] ?? null;
}

function isRawCodexPromptRole(value: unknown): boolean {
  return value === "system" || value === "developer" || value === "user";
}

function markFirstTextDelta(state: CodexTurnTraceState): number | null {
  if (state.sawTextDelta) {
    return null;
  }
  state.sawTextDelta = true;
  state.ttftMs = Date.now() - state.startedAt;
  return state.ttftMs;
}

function shellToolSucceeded(result: JsonValue): boolean {
  const exitCode = asRecord(result).exit_code;
  return typeof exitCode === "number" ? exitCode === 0 : true;
}

function shellToolResultText(result: JsonValue): string {
  const record = asRecord(result);
  if (
    typeof record.stdout === "string" ||
    typeof record.stderr === "string" ||
    typeof record.exit_code === "number"
  ) {
    return [
      `exit_code: ${record.exit_code ?? "unknown"}`,
      `stdout:\n${record.stdout ?? ""}`,
      `stderr:\n${record.stderr ?? ""}`,
    ].join("\n");
  }
  return stringifyValue(result);
}

function objectArgs(value: Record<string, unknown>): JsonObject {
  return asJsonObject(toJsonValue(value));
}

function asJsonObject(value: unknown): JsonObject {
  return isRecord(value) ? (toJsonValue(value) as JsonObject) : {};
}

function asRecord(value: unknown): Record<string, unknown> {
  return isRecord(value) ? value : {};
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function stringOrNull(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function numberField(value: unknown): number | undefined {
  return typeof value === "number" ? value : undefined;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

class CodexProtocolEventBuffer {
  private readonly entries: CodexProtocolLogEntry[] = [];

  constructor(
    private readonly eventSink: CodexEventSink,
    private readonly metadata: JsonValue | null,
  ) {}

  record(entry: CodexProtocolLogEntry): void {
    if (isCodexProtocolDelta(entry.message)) {
      return;
    }
    this.entries.push(entry);
  }

  async flush(): Promise<void> {
    if (this.entries.length === 0) {
      return;
    }
    const entries = this.entries.splice(0);
    await this.eventSink.append(
      entries.map((entry) => ({
        type: "custom",
        event_type: "codex_protocol_message",
        payload: toJsonValue({
          ...entry,
          metadata: this.metadata,
        }),
      })),
    );
  }
}

function isCodexProtocolDelta(message: JsonValue): boolean {
  if (!isRecord(message)) {
    return false;
  }
  const method = message.method;
  return typeof method === "string" && method.endsWith("/delta");
}
