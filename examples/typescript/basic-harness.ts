import {
  createToolRegistry,
  defineHarness,
  materializePromptMessages,
  registerBuiltInTools,
  registerLibraryToolsFromManifest,
  turnMetadata,
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
  const { conversation, turn } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  const tools = createToolRegistry(context);
  registerBuiltInTools(tools, context, ["shell"]);
  await registerLibraryToolsFromManifest(tools, context, {
    tools: context.agentConfig.libraryTools,
  });
  let latestEventId: string | null = null;

  for (let round = 0; ; round += 1) {
    if (
      maxToolRoundTrips !== null &&
      maxToolRoundTrips !== undefined &&
      round > maxToolRoundTrips
    ) {
      return latestEventId;
    }

    const messages = await materializePromptMessages(
      conversation,
      context.agentConfig.instructions,
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
      latestEventId = (await turn.addEvents(events)).latestEventId;
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
        latestEventId = (await turn.addEvents(toolResultEvents)).latestEventId;
      }
    }
  }
}
