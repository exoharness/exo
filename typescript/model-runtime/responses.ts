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
  toolRequestedEvent,
  type AgentConfig,
  type EventData,
  type JsonObject,
  type Message,
  type PendingToolCall,
  type ToolDefinition,
  type TurnContext,
} from "@exo/harness";

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
    events.push(messagesEvent(messages));
  }
  for (const toolCall of responseToolCalls(response)) {
    events.push(toolRequestedEvent(toolCall));
  }
  return events;
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
  return response.output
    .filter((item) => item.type === "function_call")
    .map((item) => ({
      toolCallId: item.call_id,
      request: {
        functionName: item.name,
        arguments: parseJsonObject(item.arguments),
      },
    }));
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

function parseJsonObject(json: string): JsonObject {
  const value = JSON.parse(json) as unknown;
  if (!isRecord(value)) {
    throw new Error("Responses function call arguments must be a JSON object");
  }
  return value as JsonObject;
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
