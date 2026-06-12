import OpenAI from "openai";
import {
  flush,
  initLogger,
  traced,
  type Span,
  type StartSpanArgs,
} from "braintrust";
import {
  linguaToResponsesMessages,
  responsesMessagesToLingua,
  type Message as LinguaMessage,
} from "@braintrust/lingua";
import type {
  Response,
  ResponseCreateParamsNonStreaming,
  ResponseCreateParamsStreaming,
  ResponseInput,
  ResponseStreamEvent,
  Tool,
} from "openai/resources/responses/responses";

import {
  messagesEvent,
  toolResultEvent,
  toolRequestedEvent,
  type AgentConfig,
  type EventData,
  type JsonObject,
  type Message,
  type PendingToolCall,
  type ToolDefinition,
  type TurnContext,
} from "../harness";
import { computeCostUsd, getTable } from "./cost";
import type {
  ChatCompletion,
  ChatCompletionChunk,
  ChatCompletionCreateParamsNonStreaming,
  ChatCompletionCreateParamsStreaming,
  ChatCompletionMessageParam,
  ChatCompletionMessageToolCall,
  ChatCompletionTool,
} from "openai/resources/chat/completions";

export interface NativeBraintrustOptions {
  apiKey?: string;
  appUrl?: string;
  orgName?: string;
  projectName?: string;
  projectId?: string;
}

export interface ResponsesRuntimeOptions {
  apiKey?: string;
  baseURL?: string;
  organization?: string;
  project?: string;
  braintrust?: NativeBraintrustOptions | null;
}

export interface ResponsesModelBinding {
  model?: string;
  apiKey?: string;
  baseUrl?: string | null;
}

export interface NativeResponsesRequest {
  model: string;
  messages?: Message[];
  input?: string | ResponseInput;
  tools?: ToolDefinition[];
  responseTools?: Tool[];
  maxOutputTokens?: number | null;
  metadata?: Record<string, string>;
}

export interface NativeStreamHandlers {
  onFirstChunk?: (ttftMs: number) => Promise<void> | void;
  onTextDelta?: (text: string) => Promise<void> | void;
  onStreamEvent?: (event: ResponseStreamEvent) => Promise<void> | void;
}

export type TraceParent = Span | string;

export type ToolCallExecutor = (
  toolCall: PendingToolCall,
) => Promise<EventData[]>;

export interface NativeTraceOptions {
  parent?: TraceParent;
  roundIndex?: number;
}

export interface ResponsesRuntimeLike {
  runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void>;
  complete(
    request: NativeResponsesRequest,
    options?: NativeTraceOptions,
  ): Promise<Response>;
  completeStream(
    request: NativeResponsesRequest,
    handlers?: NativeStreamHandlers,
    options?: NativeTraceOptions,
  ): Promise<Response>;
  traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute?: ToolCallExecutor,
  ): Promise<EventData[]>;
}

interface NativeLlmResult {
  response: Response;
  ttftMs: number | null;
}

interface NativeLlmTraceOptions extends NativeTraceOptions {
  streamed: boolean;
  handlers?: NativeStreamHandlers;
}

export class ResponsesRuntime implements ResponsesRuntimeLike {
  private readonly client: OpenAI;

  constructor(options: ResponsesRuntimeOptions = {}) {
    ensureBraintrustLogger(options.braintrust ?? null);
    this.client = new OpenAI({
      apiKey: options.apiKey,
      baseURL: options.baseURL,
      organization: options.organization,
      project: options.project,
    });
  }

  static fromEnvironment(agentConfig?: AgentConfig): ResponsesRuntime {
    return new ResponsesRuntime({
      apiKey: process.env.OPENAI_API_KEY,
      baseURL: process.env.OPENAI_BASE_URL,
      organization: process.env.OPENAI_ORG_ID,
      project: process.env.OPENAI_PROJECT,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  static fromModelBinding(
    agentConfig: AgentConfig | undefined,
    binding: ResponsesModelBinding,
  ): ResponsesRuntime {
    return new ResponsesRuntime({
      apiKey: binding.apiKey,
      baseURL: binding.baseUrl ?? undefined,
      organization: process.env.OPENAI_ORG_ID,
      project: process.env.OPENAI_PROJECT,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  async runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void> {
    await traceExecutorTurn(context, run);
  }

  async complete(
    request: NativeResponsesRequest,
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    const { response } = await this.runLlmRequest(request, {
      ...options,
      streamed: false,
    });
    return response;
  }

  async completeStream(
    request: NativeResponsesRequest,
    handlers: NativeStreamHandlers = {},
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    const { response } = await this.runLlmRequest(request, {
      ...options,
      streamed: true,
      handlers,
    });
    return response;
  }

  async traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute: ToolCallExecutor = (toolCall) =>
      context.executePendingTools([toolCall]),
  ): Promise<EventData[]> {
    return tracedUnderParent(
      turnParent,
      async (span) => {
        try {
          const events = await execute(toolCall);
          span.log({ output: toolResultTraceOutput(events) });
          return events;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name: toolCall.request.functionName,
        type: "tool",
        spanAttributes: { purpose: "tool_call" },
        event: {
          input: toolCall.request,
          metadata: {
            round_index: roundIndex,
          },
        },
      },
    );
  }

  private async runLlmRequest(
    request: NativeResponsesRequest,
    options: NativeLlmTraceOptions,
  ): Promise<NativeLlmResult> {
    const toolNames = (request.tools ?? []).map((tool) => tool.name);
    const run = async (span: Span): Promise<NativeLlmResult> => {
      try {
        const result = options.streamed
          ? await this.completeStreamRaw(
              buildStreamingBody(request),
              options.handlers,
            )
          : {
              response: await this.completeRaw(buildNonStreamingBody(request)),
              ttftMs: null,
            };

        span.log({
          output: llmOutputTraceValue(result.response),
          metadata: {
            response_id: result.response.id,
          },
          metrics: responseUsageMetrics(result.response, result.ttftMs),
        });
        return result;
      } catch (error) {
        span.log({ error: errorMessage(error) });
        throw error;
      }
    };
    const spanArgs = {
      name: `responses:${request.model}`,
      type: "llm" as const,
      event: {
        input: llmInputTraceValue(request),
        metadata: {
          round_index: options.roundIndex,
          runtime: "responses",
          model: request.model,
          max_output_tokens: request.maxOutputTokens ?? null,
          tool_count: toolNames.length,
          tools: toolNames,
          streamed: options.streamed,
        },
      },
    };

    return tracedUnderParent(options.parent, run, spanArgs);
  }

  private async completeRaw(
    body: ResponseCreateParamsNonStreaming,
  ): Promise<Response> {
    return this.client.responses.create(body);
  }

  private async completeStreamRaw(
    body: ResponseCreateParamsStreaming,
    handlers: NativeStreamHandlers = {},
  ): Promise<NativeLlmResult> {
    const startedAt = performance.now();
    let sawFirstChunk = false;
    let ttftMs: number | null = null;
    let finalResponse: Response | null = null;
    const stream = await this.client.responses.create(body);

    for await (const event of stream) {
      if (!sawFirstChunk) {
        sawFirstChunk = true;
        ttftMs = Math.max(0, Math.round(performance.now() - startedAt));
        await handlers.onFirstChunk?.(ttftMs);
      }

      await handlers.onStreamEvent?.(event);
      if (event.type === "response.output_text.delta") {
        await handlers.onTextDelta?.(event.delta);
      } else if (event.type === "response.completed") {
        finalResponse = event.response;
      } else if (event.type === "response.failed") {
        throw new Error(
          event.response.error?.message ?? "Responses API response failed",
        );
      }
    }

    if (!finalResponse) {
      throw new Error("Responses API stream ended without completion");
    }
    return {
      response: finalResponse,
      ttftMs,
    };
  }
}

export function runtimeFromModelBinding(
  agentConfig: AgentConfig | undefined,
  binding: ResponsesModelBinding,
): ResponsesRuntimeLike {
  return modelRequiresResponsesApi(binding.model ?? "")
    ? ResponsesRuntime.fromModelBinding(agentConfig, binding)
    : ChatCompletionsRuntime.fromModelBinding(agentConfig, binding);
}

export function modelRequiresResponsesApi(model: string): boolean {
  const lower = model.toLowerCase();
  const gpt5Minor = lower.match(/^gpt-5\.(\d+)/)?.[1]?.match(/^\d+$/)?.[0];
  return (
    lower.startsWith("o1-pro") ||
    lower.startsWith("o3-pro") ||
    lower.startsWith("gpt-5-pro") ||
    (gpt5Minor !== undefined && Number(gpt5Minor) >= 3) ||
    (lower.startsWith("gpt-5") && lower.includes("-codex"))
  );
}

export class ChatCompletionsRuntime implements ResponsesRuntimeLike {
  private readonly client: OpenAI;

  constructor(options: ResponsesRuntimeOptions = {}) {
    ensureBraintrustLogger(options.braintrust ?? null);
    this.client = new OpenAI({
      apiKey: options.apiKey,
      baseURL: options.baseURL,
      organization: options.organization,
      project: options.project,
    });
  }

  static fromModelBinding(
    agentConfig: AgentConfig | undefined,
    binding: ResponsesModelBinding,
  ): ChatCompletionsRuntime {
    return new ChatCompletionsRuntime({
      apiKey: binding.apiKey,
      baseURL: binding.baseUrl ?? undefined,
      organization: process.env.OPENAI_ORG_ID,
      project: process.env.OPENAI_PROJECT,
      braintrust: braintrustOptionsFromAgentConfig(agentConfig),
    });
  }

  async runTurn(
    context: TurnContext,
    run: (turnParent: TraceParent) => Promise<string | null>,
  ): Promise<void> {
    await traceExecutorTurn(context, run);
  }

  async complete(
    request: NativeResponsesRequest,
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    const { response } = await this.runLlmRequest(request, {
      ...options,
      streamed: false,
    });
    return response;
  }

  async completeStream(
    request: NativeResponsesRequest,
    handlers: NativeStreamHandlers = {},
    options: NativeTraceOptions = {},
  ): Promise<Response> {
    const { response } = await this.runLlmRequest(request, {
      ...options,
      streamed: true,
      handlers,
    });
    return response;
  }

  async traceToolCall(
    turnParent: TraceParent,
    context: TurnContext,
    toolCall: PendingToolCall,
    roundIndex: number,
    execute: ToolCallExecutor = (toolCall) =>
      context.executePendingTools([toolCall]),
  ): Promise<EventData[]> {
    return tracedUnderParent(
      turnParent,
      async (span) => {
        try {
          const events = await execute(toolCall);
          span.log({ output: toolResultTraceOutput(events) });
          return events;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name: toolCall.request.functionName,
        type: "tool",
        spanAttributes: { purpose: "tool_call" },
        event: {
          input: toolCall.request,
          metadata: {
            round_index: roundIndex,
          },
        },
      },
    );
  }

  private async runLlmRequest(
    request: NativeResponsesRequest,
    options: NativeLlmTraceOptions,
  ): Promise<NativeLlmResult> {
    const toolNames = (request.tools ?? []).map((tool) => tool.name);
    const run = async (span: Span): Promise<NativeLlmResult> => {
      try {
        const result = options.streamed
          ? await this.completeStreamRaw(
              buildChatStreamingBody(request),
              options.handlers,
            )
          : {
              response: chatCompletionToResponse(
                await this.completeRaw(buildChatNonStreamingBody(request)),
              ),
              ttftMs: null,
            };

        span.log({
          output: llmOutputTraceValue(result.response),
          metadata: {
            response_id: result.response.id,
          },
          metrics: responseUsageMetrics(result.response, result.ttftMs),
        });
        return result;
      } catch (error) {
        span.log({ error: errorMessage(error) });
        throw error;
      }
    };
    return tracedUnderParent(options.parent, run, {
      name: `chat:${request.model}`,
      type: "llm",
      event: {
        input: llmInputTraceValue(request),
        metadata: {
          round_index: options.roundIndex,
          runtime: "chat_completions",
          model: request.model,
          max_output_tokens: request.maxOutputTokens ?? null,
          tool_count: toolNames.length,
          tools: toolNames,
          streamed: options.streamed,
        },
      },
    });
  }

  private async completeRaw(
    body: ChatCompletionCreateParamsNonStreaming,
  ): Promise<ChatCompletion> {
    return this.client.chat.completions.create(body);
  }

  private async completeStreamRaw(
    body: ChatCompletionCreateParamsStreaming,
    handlers: NativeStreamHandlers = {},
  ): Promise<NativeLlmResult> {
    const startedAt = performance.now();
    let sawFirstChunk = false;
    let ttftMs: number | null = null;
    const accumulator = new ChatCompletionAccumulator();
    const stream = await this.client.chat.completions.create(body);

    for await (const chunk of stream) {
      if (!sawFirstChunk) {
        sawFirstChunk = true;
        ttftMs = Math.max(0, Math.round(performance.now() - startedAt));
        await handlers.onFirstChunk?.(ttftMs);
      }
      accumulator.push(chunk);
      const text = chunk.choices[0]?.delta.content;
      if (text) {
        await handlers.onTextDelta?.(text);
      }
    }

    return {
      response: accumulator.finalize(),
      ttftMs,
    };
  }
}

export async function runResponsesTurn(
  context: TurnContext,
  run: (
    runtime: ResponsesRuntimeLike,
    context: TurnContext,
    turnParent: TraceParent,
  ) => Promise<string | null>,
): Promise<void> {
  const runtime = ResponsesRuntime.fromEnvironment(context.agentConfig);
  await runtime.runTurn(context, (turnParent) =>
    run(runtime, context, turnParent),
  );
}

export async function traceExecutorTurn(
  context: TurnContext,
  run: (turnParent: TraceParent) => Promise<string | null>,
): Promise<void> {
  ensureBraintrustLogger(braintrustOptionsFromAgentConfig(context.agentConfig));
  try {
    if (context.braintrustParent) {
      await run(context.braintrustParent);
    } else {
      await traceRootExecutorTurn(context, run);
    }
  } finally {
    await flushNativeBraintrust();
  }
}

async function traceRootExecutorTurn(
  context: TurnContext,
  run: (turnParent: Span) => Promise<string | null>,
): Promise<void> {
  const { agent, conversation, turn } = context.exoharness.current;
  await traced(
    (sessionSpan) =>
      sessionSpan.traced(
        async (turnSpan) => {
          try {
            const latestEventId = await run(turnSpan);
            turnSpan.log({
              metadata: {
                status: "ok",
                latest_event_id: latestEventId,
              },
            });
          } catch (error) {
            turnSpan.log({
              error: errorMessage(error),
              metadata: { status: "error" },
            });
            throw error;
          }
        },
        {
          name: "executor_turn",
          type: "task",
          spanAttributes: { purpose: "executor_turn" },
          event: {
            metadata: {
              session_id: turn.record.sessionId,
              turn_id: turn.record.id,
              model: context.agentConfig.model,
              streamed: context.streaming,
            },
          },
        },
      ),
    {
      name: "executor_session",
      type: "task",
      spanAttributes: { purpose: "executor_session" },
      event: {
        metadata: {
          agent_id: agent.record.id,
          agent_slug: agent.record.slug,
          conversation_id: conversation.record.id,
          conversation_slug: conversation.record.slug,
          session_id: turn.record.sessionId,
          model: context.agentConfig.model,
        },
      },
    },
  );
}

function buildNonStreamingBody(
  request: NativeResponsesRequest,
): ResponseCreateParamsNonStreaming {
  return {
    model: request.model as ResponseCreateParamsNonStreaming["model"],
    input: request.input ?? linguaMessagesToResponsesInput(request.messages),
    tools:
      request.responseTools ??
      toolDefinitionsToResponsesTools(request.tools ?? []),
    max_output_tokens: request.maxOutputTokens ?? null,
    metadata: request.metadata ?? null,
    stream: false,
    store: false,
  };
}

function buildStreamingBody(
  request: NativeResponsesRequest,
): ResponseCreateParamsStreaming {
  return {
    model: request.model as ResponseCreateParamsStreaming["model"],
    input: request.input ?? linguaMessagesToResponsesInput(request.messages),
    tools:
      request.responseTools ??
      toolDefinitionsToResponsesTools(request.tools ?? []),
    max_output_tokens: request.maxOutputTokens ?? null,
    metadata: request.metadata ?? null,
    stream: true,
    store: false,
  };
}

function buildChatNonStreamingBody(
  request: NativeResponsesRequest,
): ChatCompletionCreateParamsNonStreaming {
  const tools = toolDefinitionsToChatTools(request.tools ?? []);
  return {
    model: request.model,
    messages: messagesToChatMessages(request.messages ?? []),
    tools: tools.length === 0 ? undefined : tools,
    tool_choice: tools.length === 0 ? undefined : "auto",
    max_tokens: request.maxOutputTokens ?? undefined,
    stream: false,
  };
}

function buildChatStreamingBody(
  request: NativeResponsesRequest,
): ChatCompletionCreateParamsStreaming {
  const tools = toolDefinitionsToChatTools(request.tools ?? []);
  return {
    model: request.model,
    messages: messagesToChatMessages(request.messages ?? []),
    tools: tools.length === 0 ? undefined : tools,
    tool_choice: tools.length === 0 ? undefined : "auto",
    max_tokens: request.maxOutputTokens ?? undefined,
    stream: true,
    stream_options: { include_usage: true },
  };
}

function messagesToChatMessages(
  messages: Message[],
): ChatCompletionMessageParam[] {
  return messages.map(messageToChatMessage);
}

function messageToChatMessage(message: Message): ChatCompletionMessageParam {
  if (message.role === "system" || message.role === "developer") {
    return { role: "system", content: messageContentText(message.content) };
  }
  if (message.role === "user") {
    return { role: "user", content: messageContentText(message.content) };
  }
  if (message.role === "tool") {
    const result = toolResultContent(message.content);
    return {
      role: "tool",
      tool_call_id: result.toolCallId,
      content: JSON.stringify(result.output),
    };
  }
  const toolCalls = assistantToolCalls(message.content);
  return {
    role: "assistant",
    content: assistantTextContent(message.content),
    tool_calls: toolCalls.length === 0 ? undefined : toolCalls,
  };
}

function toolDefinitionsToChatTools(
  tools: ToolDefinition[],
): ChatCompletionTool[] {
  return tools.map((tool) => ({
    type: "function",
    function: {
      name: tool.name,
      description: tool.description,
      parameters: tool.parameters as JsonObject,
      strict: true,
    },
  }));
}

function chatCompletionToResponse(completion: ChatCompletion): Response {
  const choice = completion.choices[0];
  const output: unknown[] = [];
  if (choice?.message.content) {
    output.push(
      responseMessageOutput(`${completion.id}_message`, choice.message.content),
    );
  }
  for (const toolCall of choice?.message.tool_calls ?? []) {
    if (toolCall.type === "function") {
      output.push(responseFunctionCallOutput(toolCall));
    }
  }
  return {
    id: completion.id,
    object: "response",
    created_at: completion.created,
    status: "completed",
    model: completion.model,
    output,
    usage: chatUsageToResponseUsage(completion.usage),
  } as unknown as Response;
}

class ChatCompletionAccumulator {
  private id = `chatcmpl_${Date.now()}`;
  private created = Math.floor(Date.now() / 1000);
  private model = "";
  private content = "";
  private usage: ChatCompletionChunk["usage"] | null = null;
  private readonly toolCalls = new Map<
    number,
    {
      id?: string;
      name?: string;
      arguments: string;
    }
  >();

  push(chunk: ChatCompletionChunk): void {
    this.id = chunk.id || this.id;
    this.created = chunk.created || this.created;
    this.model = chunk.model || this.model;
    this.usage = chunk.usage ?? this.usage;
    for (const choice of chunk.choices) {
      const delta = choice.delta;
      if (delta.content) {
        this.content += delta.content;
      }
      for (const toolCall of delta.tool_calls ?? []) {
        const index = toolCall.index;
        const current = this.toolCalls.get(index) ?? { arguments: "" };
        current.id = toolCall.id ?? current.id;
        current.name = toolCall.function?.name ?? current.name;
        current.arguments += toolCall.function?.arguments ?? "";
        this.toolCalls.set(index, current);
      }
    }
  }

  finalize(): Response {
    const output: unknown[] = [];
    if (this.content.length > 0) {
      output.push(responseMessageOutput(`${this.id}_message`, this.content));
    }
    for (const [, toolCall] of [...this.toolCalls.entries()].sort(
      ([left], [right]) => left - right,
    )) {
      if (!toolCall.id || !toolCall.name) {
        continue;
      }
      output.push(
        responseFunctionCallOutput({
          id: toolCall.id,
          type: "function",
          function: {
            name: toolCall.name,
            arguments: toolCall.arguments,
          },
        } as ChatFunctionToolCall),
      );
    }
    return {
      id: this.id,
      object: "response",
      created_at: this.created,
      status: "completed",
      model: this.model,
      output,
      usage: chatUsageToResponseUsage(this.usage),
    } as unknown as Response;
  }
}

function responseMessageOutput(id: string, text: string): unknown {
  return {
    id,
    type: "message",
    role: "assistant",
    status: "completed",
    content: [
      {
        type: "output_text",
        text,
        annotations: [],
      },
    ],
  };
}

type ChatFunctionToolCall = Extract<
  ChatCompletionMessageToolCall,
  { type: "function" }
>;

function responseFunctionCallOutput(toolCall: ChatFunctionToolCall): unknown {
  return {
    id: `${toolCall.id}_item`,
    type: "function_call",
    call_id: toolCall.id,
    name: toolCall.function.name,
    arguments: toolCall.function.arguments,
    status: "completed",
  };
}

function chatUsageToResponseUsage(
  usage:
    | ChatCompletion["usage"]
    | ChatCompletionChunk["usage"]
    | null
    | undefined,
): unknown {
  if (!usage) {
    return null;
  }
  return {
    input_tokens: usage.prompt_tokens,
    output_tokens: usage.completion_tokens,
    total_tokens: usage.total_tokens,
    input_tokens_details: {
      cached_tokens: usage.prompt_tokens_details?.cached_tokens ?? 0,
    },
    output_tokens_details: {
      reasoning_tokens: usage.completion_tokens_details?.reasoning_tokens ?? 0,
    },
  };
}

function assistantToolCalls(content: unknown): ChatCompletionMessageToolCall[] {
  if (!Array.isArray(content)) {
    return [];
  }
  return content.flatMap((part): ChatCompletionMessageToolCall[] => {
    if (!isRecord(part) || part.type !== "tool_call") {
      return [];
    }
    if (
      typeof part.tool_call_id !== "string" ||
      typeof part.tool_name !== "string"
    ) {
      return [];
    }
    return [
      {
        id: part.tool_call_id,
        type: "function",
        function: {
          name: part.tool_name,
          arguments: JSON.stringify(
            isRecord(part.arguments) ? part.arguments : {},
          ),
        },
      },
    ];
  });
}

function assistantTextContent(content: unknown): string | null {
  if (Array.isArray(content)) {
    const text = content
      .filter((part) => isRecord(part) && part.type === "text")
      .map((part) => messageContentText((part as { text?: unknown }).text))
      .join("");
    return text || null;
  }
  return messageContentText(content);
}

function toolResultContent(content: unknown): {
  toolCallId: string;
  output: unknown;
} {
  const part = Array.isArray(content) ? content.find(isRecord) : null;
  if (
    !isRecord(part) ||
    part.type !== "tool_result" ||
    typeof part.tool_call_id !== "string"
  ) {
    throw new Error("tool message must contain a tool_result content part");
  }
  return {
    toolCallId: part.tool_call_id,
    output: part.output,
  };
}

function messageContentText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (content === null || content === undefined) {
    return "";
  }
  return JSON.stringify(content);
}

export function tracedUnderParent<R>(
  parent: TraceParent | undefined,
  run: (span: Span) => R,
  args: StartSpanArgs,
): R {
  if (!parent) {
    return traced(run, args);
  }
  if (typeof parent === "string") {
    return traced(run, { ...args, parent });
  }
  return parent.traced(run, args);
}

export function linguaMessagesToResponsesInput(
  messages: Message[] | undefined,
): ResponseInput {
  return linguaToResponsesMessages<ResponseInput>(
    (messages ?? []) as LinguaMessage[],
  );
}

export function responseToLinguaEvents(response: Response): EventData[] {
  const events: EventData[] = [];
  const messages = responseMessages(response);
  if (messages.length > 0) {
    events.push(messagesEvent(messages, undefined, usageRecord(response)));
  }
  for (const result of responseToolCallResults(response)) {
    if (result.type === "tool_call") {
      events.push(toolRequestedEvent(result.toolCall));
    } else {
      events.push(
        toolResultEvent(result.toolCallId, {
          ok: false,
          error: result.error,
        }),
      );
    }
  }
  return events;
}

// Policy: attach raw usage + cost to the messages event. cost_usd is filled from
// the shared price cache; left unset if the cache is unavailable.
function usageRecord(response: Response): JsonObject | undefined {
  const usage = response.usage;
  if (!usage) return undefined;
  const prompt = usage.input_tokens;
  const completion = usage.output_tokens;
  const cached = usage.input_tokens_details?.cached_tokens;
  const reasoning = usage.output_tokens_details?.reasoning_tokens;
  const table = getTable();
  const cost = table
    ? computeCostUsd(table, response.model, { prompt, completion, cached })
    : null;

  const record: JsonObject = { model: response.model };
  if (prompt != null) record.prompt_tokens = prompt;
  if (completion != null) record.completion_tokens = completion;
  if (cached != null) record.prompt_cached_tokens = cached;
  if (reasoning != null) record.completion_reasoning_tokens = reasoning;
  if (cost != null) record.cost_usd = cost;
  return record;
}

export function responseStreamEventToLinguaEvents(
  event: ResponseStreamEvent,
): EventData[] {
  return event.type === "response.completed"
    ? responseToLinguaEvents(event.response)
    : [];
}

export function responseMessages(response: Response): Message[] {
  return responsesMessagesToLingua(response.output) as Message[];
}

export function responseToolCalls(response: Response): PendingToolCall[] {
  return responseToolCallResults(response).flatMap((result) =>
    result.type === "tool_call" ? [result.toolCall] : [],
  );
}

type ResponseToolCallResult =
  | {
      type: "tool_call";
      toolCall: PendingToolCall;
    }
  | {
      type: "parse_error";
      toolCallId: string;
      error: string;
    };

function responseToolCallResults(response: Response): ResponseToolCallResult[] {
  return response.output
    .filter((item) => item.type === "function_call")
    .map((item) => {
      const parsed = parseJsonObject(item.arguments);
      if (!parsed.ok) {
        return {
          type: "parse_error",
          toolCallId: item.call_id,
          error: `Invalid JSON arguments for ${item.name}: ${parsed.error}`,
        };
      }
      return {
        type: "tool_call",
        toolCall: {
          toolCallId: item.call_id,
          request: {
            functionName: item.name,
            arguments: parsed.value,
          },
        },
      };
    });
}

export function toolDefinitionsToResponsesTools(
  tools: ToolDefinition[],
): Tool[] {
  return tools.map((tool) => ({
    type: "function",
    name: tool.name,
    description: tool.description,
    parameters: tool.parameters as JsonObject,
    strict: true,
  }));
}

export async function flushNativeBraintrust(): Promise<void> {
  await flush();
}

let initializedBraintrustKey: string | null = null;

function ensureBraintrustLogger(options: NativeBraintrustOptions | null): void {
  const apiKey = options?.apiKey ?? process.env.BRAINTRUST_API_KEY;
  if (!apiKey) {
    return;
  }

  const loggerOptions = {
    apiKey,
    appUrl: options?.appUrl ?? process.env.BRAINTRUST_APP_URL,
    orgName: options?.orgName,
    projectName: options?.projectName,
    projectId: options?.projectId,
    asyncFlush: true,
  };
  const key = JSON.stringify(loggerOptions);
  if (initializedBraintrustKey === key) {
    return;
  }
  initLogger(loggerOptions);
  initializedBraintrustKey = key;
}

function braintrustOptionsFromAgentConfig(
  agentConfig: AgentConfig | undefined,
): NativeBraintrustOptions | null {
  const raw = agentConfig?.braintrust;
  if (!isRecord(raw)) {
    return null;
  }

  const project = raw.project;
  const options: NativeBraintrustOptions = {
    apiKey: process.env.BRAINTRUST_API_KEY,
    appUrl: process.env.BRAINTRUST_APP_URL,
    orgName: stringField(raw, "org_name") ?? stringField(raw, "orgName"),
  };

  if (isRecord(project)) {
    const kind = stringField(project, "kind");
    const value = stringField(project, "value");
    if (kind === "name") {
      options.projectName = value;
    } else if (kind === "id") {
      options.projectId = value;
    }
  }

  return options;
}

function llmInputTraceValue(request: NativeResponsesRequest): unknown {
  return request.messages ?? request.input ?? null;
}

function llmOutputTraceValue(response: Response): Record<string, unknown> {
  return {
    messages: responseMessages(response),
    tool_calls: responseToolCalls(response),
    status: response.status,
  };
}

function responseUsageMetrics(
  response: Response,
  ttftMs: number | null,
): Record<string, number> {
  const metrics: Record<string, number> = {};
  const usage = response.usage;
  if (usage) {
    metrics.prompt_tokens = usage.input_tokens;
    metrics.completion_tokens = usage.output_tokens;
    metrics.tokens = usage.total_tokens;
    metrics.prompt_cached_tokens = usage.input_tokens_details.cached_tokens;
    metrics.completion_reasoning_tokens =
      usage.output_tokens_details.reasoning_tokens;
  }
  if (ttftMs !== null) {
    metrics.time_to_first_token = ttftMs / 1000;
  }
  return metrics;
}

function toolResultTraceOutput(events: EventData[]): unknown {
  const results = events
    .filter((event) => event.type === "tool_result")
    .map((event) => event.result);
  return results.length === 1 ? results[0] : results;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

type JsonObjectParseResult =
  | { ok: true; value: JsonObject }
  | { ok: false; error: string };

function parseJsonObject(json: string): JsonObjectParseResult {
  try {
    const value = JSON.parse(json) as unknown;
    if (!isRecord(value)) {
      return {
        ok: false,
        error: "function call arguments must be a JSON object",
      };
    }
    return { ok: true, value: value as JsonObject };
  } catch (error) {
    return {
      ok: false,
      error: errorMessage(error),
    };
  }
}

function stringField(
  record: Record<string, unknown>,
  key: string,
): string | undefined {
  const value = record[key];
  return typeof value === "string" ? value : undefined;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
