import {
  createToolRegistry,
  defineHarness,
  materializePromptMessages,
  registerBuiltInTools,
  registerAgentToolsFromDirectoryIfExists,
  turnMetadata,
  type EventData,
  type Message,
  type TurnContext,
} from "@exo/harness";
import {
  responseToLinguaEvents,
  responseToolCalls,
  ResponsesRuntime,
  type NativeResponsesRequest,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";

import { resolveLlmBinding } from "./shared";

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    const runtime = ResponsesRuntime.fromModelBinding(
      context.agentConfig,
      modelBinding,
    );
    await runtime.runTurn(context, (turnParent) =>
      runBasicTurnLoop(runtime, context, turnParent, modelBinding.model),
    );
  },
});

async function runBasicTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  model: string,
): Promise<string | null> {
  const { conversation } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  let latestEventId: string | null = null;

  for (let round = 0; ; round += 1) {
    if (
      maxToolRoundTrips !== null &&
      maxToolRoundTrips !== undefined &&
      round > maxToolRoundTrips
    ) {
      return latestEventId;
    }

    const tools = await createBasicToolRegistry(context);
    const messages = await materializePromptMessages(
      conversation,
      basicHarnessInstructions(context),
    );
    const request: NativeResponsesRequest = {
      model,
      messages,
      tools: tools.definitions(),
      maxOutputTokens: context.agentConfig.maxOutputTokens,
      metadata: turnMetadata(context),
    };

    const response = context.streaming
      ? await runtime.completeStream(
          request,
          {
            onFirstChunk: (ttftMs) => context.stream.firstChunk(ttftMs),
            onTextDelta: (text) => context.stream.text(text),
          },
          {
            parent: turnParent,
            roundIndex: round,
          },
        )
      : await runtime.complete(request, {
          parent: turnParent,
          roundIndex: round,
        });

    const events = responseToLinguaEvents(response);
    if (events.length > 0) {
      latestEventId = await appendTurnEvents(context, events);
    }

    const toolCalls = responseToolCalls(response);
    if (toolCalls.length === 0) {
      return latestEventId;
    }

    for (const toolCall of toolCalls) {
      const toolResultEvents = await runtime.traceToolCall(
        turnParent,
        context,
        toolCall,
        round,
        (toolCall) => tools.executePending([toolCall]),
      );
      if (toolResultEvents.length > 0) {
        latestEventId = await appendTurnEvents(context, toolResultEvents);
      }
    }
  }
}

async function appendTurnEvents(
  context: TurnContext,
  data: EventData[],
): Promise<string> {
  const { conversation, turn } = context.exoharness.current;
  return (
    await conversation.addEvents({
      sessionId: turn.record.sessionId,
      turnId: turn.record.id,
      data,
    })
  ).latestEventId;
}

function basicHarnessInstructions(context: TurnContext): Message[] {
  return context.agentConfig.enableAgentToolCreation
    ? [...context.agentConfig.instructions, agentToolCreationInstruction()]
    : context.agentConfig.instructions;
}

function agentToolCreationInstruction(): Message {
  return {
    role: "developer",
    content:
      "Agent-created tools are supported. When the user asks you to create a reusable tool, call install_agent_tool with a complete TypeScript moduleSource. Do not claim the tool was created unless install_agent_tool returns ok: true. The moduleSource must use type-only imports from @exo/harness/tool and default-export a Tool using { definition, initializationParameters, initialize(...) } satisfies Tool; definition.parameters must be a strict JSON schema object with additionalProperties: false; handlers must implement execute(args, execution), not invoke or call. Do not use zod, inputSchema, external npm packages, or runtime imports from @exo/harness/tool. After install_agent_tool succeeds, the new tool is available in the next model round of the same turn, so use it directly rather than falling back to shell.",
  };
}

async function createBasicToolRegistry(context: TurnContext) {
  const tools = createToolRegistry(context);
  registerBuiltInTools(tools, context, builtInToolNames(context));
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromDirectoryIfExists(tools, context);
  }
  return tools;
}

function builtInToolNames(
  context: TurnContext,
): Array<"shell" | "install_agent_tool"> {
  return context.agentConfig.enableAgentToolCreation
    ? ["shell", "install_agent_tool"]
    : ["shell"];
}
