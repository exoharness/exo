import {
  createToolRegistry,
  messagesEvent,
  registerAgentToolsFromDirectoryIfExists,
  registerBuiltInTools,
  registerLibraryToolModulePath,
  turnMetadata,
  userTextMessage,
  type BuiltInToolName,
  type EventData,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";
import {
  responseToLinguaEvents,
  responseToolCalls,
  runtimeFromModelBinding,
  type NativeResponsesRequest,
  type ResponsesRuntimeLike,
  type TraceParent,
} from "@exo/model-runtime/responses";
import { ensureTable } from "@exo/model-runtime/cost";

import { materializeWorkerclawPromptMessages } from "./message-materialize.js";
import {
  buildTextOnlyNudgeMessage,
  extractAssistantTextFromEvents,
  isRoundBudgetExhausted,
  resolveMaxTextOnlyNudges,
  shouldExitOnTextOnly,
} from "./turn-loop-nudge.js";
import { resolveLlmBinding } from "../typescript/shared.js";
import {
  buildRoundBudgetContinueMessage,
  DEFAULT_ROUND_BUDGET_EXTENSIONS,
  isTaskTreeFinished,
  readTaskTreeSnapshot,
} from "./task-tree-snapshot.js";

export interface WorkerclawTurnLoopOptions {
  instructions?: (context: TurnContext) => Message[] | Promise<Message[]>;
  registerTools?: (
    tools: HarnessToolRegistry,
    context: TurnContext,
  ) => Promise<void> | void;
}

export async function runWorkerclawHarnessTurn(
  context: TurnContext,
  options: WorkerclawTurnLoopOptions = {},
): Promise<void> {
  await ensureTable();
  const modelBinding = await resolveLlmBinding(context);
  const runtime = runtimeFromModelBinding(context.agentConfig, modelBinding);
  await runtime.runTurn(context, (turnParent) =>
    runWorkerclawTurnLoop(
      runtime,
      context,
      turnParent,
      modelBinding.model,
      options,
    ),
  );
}

export function defaultBuiltInToolNames(
  context: TurnContext,
): BuiltInToolName[] {
  const names: BuiltInToolName[] = ["shell"];
  if (context.agentConfig.enableAgentToolCreation) {
    names.push("install_agent_tool", "uninstall_agent_tool");
  }
  return names;
}

export function basicHarnessInstructions(context: TurnContext): Message[] {
  return context.agentConfig.enableAgentToolCreation
    ? [...context.agentConfig.instructions, agentToolCreationInstruction()]
    : context.agentConfig.instructions;
}

export function agentToolCreationInstruction(): Message {
  return {
    role: "developer",
    content: [
      "install_agent_tool is available this turn. Use it when you need a named, reusable helper that you will call again (or that clarifies a multi-step workflow).",
      "Good triggers: wrapping an HTTP API with fetch, parsing/validating a recurring format, packaging a multi-command workflow into one tool, or bridging two platform tools with custom glue.",
      "Prefer an existing Olivia catalog tool when it already does the job. Prefer e2b_run_command / shell for a true one-shot. Prefer install_agent_tool when the same logic would otherwise be copy-pasted across rounds.",
      "Call install_agent_tool with a complete TypeScript moduleSource. Do not claim success unless it returns ok: true. The new tool is available in the next model round of the same turn.",
      "moduleSource rules: type-only imports from @exo/harness/tool; default-export { definition, initializationParameters, initialize(...) } satisfies Tool; definition.parameters must be a strict JSON schema object with additionalProperties: false; handlers implement execute(args, execution) (not invoke/call); no zod, no external npm packages, no runtime imports from @exo/harness/tool.",
      "Use uninstall_agent_tool to remove obsolete or conflicting agent-created tools.",
    ].join(" "),
  };
}

async function runWorkerclawTurnLoop(
  runtime: ResponsesRuntimeLike,
  context: TurnContext,
  turnParent: TraceParent,
  model: string,
  options: WorkerclawTurnLoopOptions,
): Promise<string | null> {
  const { conversation } = context.exoharness.current;
  const maxToolRoundTrips = context.agentConfig.maxToolRoundTrips;
  const maxTextOnlyNudges = resolveMaxTextOnlyNudges();
  let latestEventId: string | null = null;
  let budgetExtensions = 0;
  let completeTaskCalled = false;
  let textOnlyNudgesUsed = 0;

  for (let round = 0; ; round += 1) {
    if (
      isRoundBudgetExhausted(
        round,
        maxToolRoundTrips,
        maxTextOnlyNudges,
        completeTaskCalled,
      )
    ) {
      const snapshot = await readTaskTreeSnapshot(context);
      if (isTaskTreeFinished(snapshot)) {
        return latestEventId;
      }
      if (budgetExtensions >= DEFAULT_ROUND_BUDGET_EXTENSIONS) {
        console.warn(
          `[workerclaw] round budget exhausted before complete_task (round=${round}, maxToolRoundTrips=${maxToolRoundTrips ?? "none"}, nudgesUsed=${textOnlyNudgesUsed})`,
        );
        return latestEventId;
      }
      budgetExtensions += 1;
      round = 0;
      latestEventId = await appendTurnEvents(context, [
        messagesEvent([
          userTextMessage(
            buildRoundBudgetContinueMessage(
              budgetExtensions,
              DEFAULT_ROUND_BUDGET_EXTENSIONS,
            ),
          ),
        ]),
      ]);
      continue;
    }

    const tools = createToolRegistry(context);
    if (options.registerTools) {
      await options.registerTools(tools, context);
    } else {
      registerBuiltInTools(tools, context, defaultBuiltInToolNames(context));
      for (const modulePath of context.agentConfig.typescript
        ?.toolModulePaths ?? []) {
        await registerLibraryToolModulePath(tools, context, modulePath);
      }
      if (context.agentConfig.enableAgentToolCreation) {
        await registerAgentToolsFromDirectoryIfExists(tools, context);
      }
    }

    const messages = await materializeWorkerclawPromptMessages(
      conversation,
      options.instructions
        ? await options.instructions(context)
        : basicHarnessInstructions(context),
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
    const hasSyntheticToolResult = events.some(
      (event) => event.type === "tool_result",
    );
    if (toolCalls.length === 0) {
      if (hasSyntheticToolResult) {
        continue;
      }
      const snapshot = await readTaskTreeSnapshot(context);
      if (isTaskTreeFinished(snapshot)) {
        return latestEventId;
      }
      if (
        shouldExitOnTextOnly(
          completeTaskCalled,
          textOnlyNudgesUsed,
          maxTextOnlyNudges,
        )
      ) {
        return latestEventId;
      }

      textOnlyNudgesUsed += 1;
      const lastAssistantText = extractAssistantTextFromEvents(events);
      const nudge = buildTextOnlyNudgeMessage(
        textOnlyNudgesUsed,
        lastAssistantText,
      );
      console.warn(
        `[workerclaw] text-only exit before complete_task — nudge ${textOnlyNudgesUsed}/${maxTextOnlyNudges}`,
      );
      latestEventId = await appendTurnEvents(context, [
        messagesEvent([
          {
            role: "developer",
            content: nudge,
          },
        ]),
      ]);
      continue;
    }

    for (const toolCall of toolCalls) {
      if (toolCall.request.functionName === "complete_task") {
        completeTaskCalled = true;
      }
      const toolResultEvents = await runtime.traceToolCall(
        turnParent,
        context,
        toolCall,
        round,
        (pending) => tools.executePending([pending]),
      );
      if (toolResultEvents.length > 0) {
        latestEventId = await appendTurnEvents(context, toolResultEvents);
      }
    }

    if (isTaskTreeFinished(await readTaskTreeSnapshot(context))) {
      return latestEventId;
    }
  }
}

async function appendTurnEvents(
  context: TurnContext,
  data: EventData[],
): Promise<string> {
  return (await context.exoharness.current.turn.addEvents(data)).latestEventId;
}
