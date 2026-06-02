import {
  appendCustomEvent,
  defineHarness,
  toJsonValue,
  toolRequestedEvent,
  toolResultEvent,
  turnMetadata,
  type EventData,
  type JsonValue,
  type PendingToolCall,
  type TurnContext,
} from "@exo/harness";
import { CodexAppServer } from "@exo/codex/app-server";
import {
  CODEX_EXO_SHELL_DYNAMIC_TOOL,
  CODEX_WARM_SESSION_EVENT,
  codexWarmSessionRecord,
  runCodexTurn,
  traceOutputPreview,
  type CodexAppServerProvider,
  type CodexAppServerResource,
  type CodexAppServerStartOptions,
  type CodexEventSink,
  type CodexLlmTraceDetails,
  type CodexLlmTraceLog,
  type CodexSandboxProcessRef,
  type CodexStreamSink,
  type CodexToolSink,
  type CodexTraceSink,
  type CodexTurnRequest,
  type CodexWarmSessionRecord,
  type CodexWarmSessionStore,
} from "@exo/codex/turn-runner";
import {
  errorMessage,
  ResponsesRuntime,
  tracedUnderParent,
  type TraceParent,
} from "@exo/model-runtime/responses";

import {
  appendEvents,
  instructionsText,
  materializePriorConversationMessages,
  pickEnv,
  resolveLlmBinding,
  sandboxCwd,
  traceExoharnessToolCall,
  traceObservedToolCall,
  type ResolvedLlmBinding,
} from "./shared";

export default defineHarness({
  async runTurn(context) {
    const modelBinding = await resolveLlmBinding(context);
    const runtime = ResponsesRuntime.fromModelBinding(
      context.agentConfig,
      modelBinding,
    );
    await runtime.runTurn(context, async (turnParent) => {
      await requireCodexSandboxNetworking(context);
      await runCodexTurn(
        await codexTurnRequest(context, modelBinding),
        codexTurnCapabilities(context, turnParent, modelBinding),
      );
      return null;
    });
  },
});

async function codexTurnRequest(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
): Promise<CodexTurnRequest> {
  const sandboxRuntimeKey = codexSandboxRuntimeKey(context);
  return {
    input: context.request.input,
    priorMessages: await materializePriorConversationMessages(context),
    model: modelBinding.model,
    modelProvider: "openai",
    developerInstructions: instructionsText(context.agentConfig.instructions),
    cwd: codexAppServerCwd(context),
    sandboxRuntimeKey,
    warmSessionKey: codexWarmSessionKey(
      context,
      modelBinding,
      sandboxRuntimeKey,
    ),
    dynamicTools: buildCodexDynamicTools(context),
    sandboxPolicy: codexNativeSandboxPolicy(),
    externalSandbox: useCodexExternalSandbox(),
    metadata: toJsonValue(turnMetadata(context)),
    streaming: context.streaming,
  };
}

function codexTurnCapabilities(
  context: TurnContext,
  turnParent: TraceParent,
  modelBinding: ResolvedLlmBinding,
): {
  appServer: CodexAppServerProvider;
  eventSink: CodexEventSink;
  streamSink: CodexStreamSink;
  toolSink: CodexToolSink;
  warmSessionStore: CodexWarmSessionStore;
  trace: CodexTraceSink;
} {
  return {
    appServer: new ExoCodexAppServerProvider(context, modelBinding),
    eventSink: new ExoCodexEventSink(context),
    streamSink: new ExoCodexStreamSink(context),
    toolSink: new ExoCodexToolSink(context, turnParent),
    warmSessionStore: new ExoCodexWarmSessionStore(context),
    trace: new ExoCodexTraceSink(context, turnParent),
  };
}

class ExoCodexAppServerProvider implements CodexAppServerProvider {
  constructor(
    private readonly context: TurnContext,
    private readonly modelBinding: ResolvedLlmBinding,
  ) {}

  async start(
    options: CodexAppServerStartOptions,
  ): Promise<CodexAppServerResource> {
    const process = await this.context.startSandboxProcess({
      command: codexSandboxCommand(this.context),
      env: codexSandboxEnv(this.modelBinding),
      reuseKey: options.sessionKey,
    });
    const serverOptions = {
      process,
      onProtocolMessage: options.onProtocolMessage,
      onServerRequest: options.onServerRequest,
    };
    const server = process.reused
      ? await CodexAppServer.attachToSandbox(serverOptions)
      : await CodexAppServer.startInSandbox(serverOptions);
    return { server, process };
  }
}

class ExoCodexEventSink implements CodexEventSink {
  constructor(private readonly context: TurnContext) {}

  async append(data: EventData[]): Promise<void> {
    await appendEvents(this.context, data);
  }

  async appendCustom(eventType: string, payload: JsonValue): Promise<void> {
    await appendCustomEvent(
      this.context.exoharness.current.turn,
      eventType,
      payload,
    );
  }
}

class ExoCodexStreamSink implements CodexStreamSink {
  constructor(private readonly context: TurnContext) {}

  async firstChunk(ttftMs: number): Promise<void> {
    await this.context.stream.firstChunk(ttftMs);
  }

  async text(delta: string): Promise<void> {
    await this.context.stream.text(delta);
  }

  async status(message: string): Promise<void> {
    await this.context.stream.text(`[codex] ${message}\n`);
  }
}

class ExoCodexToolSink implements CodexToolSink {
  constructor(
    private readonly context: TurnContext,
    private readonly turnParent: TraceParent,
  ) {}

  async call(toolCall: PendingToolCall): Promise<JsonValue> {
    await appendEvents(this.context, [toolRequestedEvent(toolCall)]);
    try {
      const result = await traceExoharnessToolCall(
        this.context,
        this.turnParent,
        toolCall,
        "codex_dynamic_tool",
      );
      await appendEvents(this.context, [
        toolResultEvent(toolCall.toolCallId, result),
      ]);
      return result;
    } catch (error) {
      const message = errorMessage(error);
      await appendEvents(this.context, [
        toolResultEvent(toolCall.toolCallId, toJsonValue({ error: message })),
      ]);
      throw error;
    }
  }

  async observe(toolCall: PendingToolCall, result: JsonValue): Promise<void> {
    await traceObservedToolCall(
      this.context,
      this.turnParent,
      toolCall,
      result,
      "codex_observed_tool",
    );
  }
}

class ExoCodexWarmSessionStore implements CodexWarmSessionStore {
  constructor(private readonly context: TurnContext) {}

  async latest(
    sessionKey: string,
    process: CodexSandboxProcessRef,
  ): Promise<CodexWarmSessionRecord | null> {
    const result = await this.context.exoharness.current.conversation.getEvents(
      {
        direction: "desc",
        limit: 100,
        types: [CODEX_WARM_SESSION_EVENT],
      },
    );
    for (const event of result.events) {
      const record = codexWarmSessionRecord(event.data);
      if (
        record?.sessionKey === sessionKey &&
        (!process.sandboxId || record.sandboxId === process.sandboxId) &&
        (!process.sandboxProcessId ||
          record.sandboxProcessId === process.sandboxProcessId)
      ) {
        return record;
      }
    }
    return null;
  }

  async record(
    sessionKey: string,
    process: CodexSandboxProcessRef,
    threadId: string,
  ): Promise<void> {
    await appendCustomEvent(
      this.context.exoharness.current.turn,
      CODEX_WARM_SESSION_EVENT,
      {
        sessionKey,
        sandboxId: process.sandboxId ?? null,
        sandboxProcessId: process.sandboxProcessId ?? null,
        threadId,
      },
    );
  }
}

class ExoCodexTraceSink implements CodexTraceSink {
  constructor(
    private readonly context: TurnContext,
    private readonly turnParent: TraceParent,
  ) {}

  async task<R>(
    name: string,
    input: unknown,
    run: () => Promise<R>,
  ): Promise<R> {
    return tracedUnderParent(
      this.turnParent,
      async (span) => {
        try {
          const result = await run();
          span.log({ output: traceOutputPreview(result) });
          return result;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name,
        type: "task",
        spanAttributes: { purpose: "codex_app_server" },
        event: { input },
      },
    );
  }

  async llmTurn<R>(
    details: CodexLlmTraceDetails,
    run: () => Promise<R>,
    buildLog: () => CodexLlmTraceLog,
  ): Promise<R> {
    return tracedUnderParent(
      this.turnParent,
      async (span) => {
        try {
          const result = await run();
          span.log(buildLog());
          return result;
        } catch (error) {
          span.log({ error: errorMessage(error) });
          throw error;
        }
      },
      {
        name: details.name,
        type: "llm",
        spanAttributes: { purpose: "codex_llm_turn" },
        event: {
          input: details.input,
          metadata: {
            ...turnMetadata(this.context),
            runtime: "codex_app_server",
            model: details.model,
            codex_thread_id: details.threadId,
            injected_response_items: details.injectedResponseItems,
            streamed: details.streamed,
          },
        },
      },
    );
  }
}

async function requireCodexSandboxNetworking(
  context: TurnContext,
): Promise<void> {
  if (codexEffectiveNetworking(context)) {
    return;
  }
  await appendCustomEvent(
    context.exoharness.current.turn,
    "codex_networking_required",
    {
      metadata: turnMetadata(context),
      agent_enable_networking: context.agentConfig.enableNetworking,
      reason:
        "Codex runs its model stream inside the exoharness sandbox, so the agent sandbox must have networking enabled.",
    },
  );
  throw new Error(codexSandboxNetworkingError(context));
}

function codexSandboxNetworkingError(context: TurnContext): string {
  return [
    "Codex requires agent networking because it runs model calls inside the exoharness sandbox.",
    `Enable it with: exo agent update ${context.exoharness.current.agent.record.slug} --networking enabled`,
  ].join(" ");
}

function codexEffectiveNetworking(context: TurnContext): boolean {
  return context.agentConfig.enableNetworking;
}

function codexSandboxCommand(context: TurnContext): string[] {
  const shell = context.conversationConfig.shellProgram ?? "/bin/bash";
  const command = [
    "set -e;",
    'mkdir -p "${HOME:-/tmp/exo-home}" "${CODEX_HOME:-/tmp/exo-codex-home}" >/dev/null 2>/tmp/codex-setup.stderr;',
    'if [ -n "${OPENAI_API_KEY:-}" ] && [ ! -f "${CODEX_HOME:-/tmp/exo-codex-home}/auth.json" ]; then',
    'printf "%s" "$OPENAI_API_KEY" | codex login --with-api-key >/dev/null 2>/tmp/codex-login.stderr;',
    "fi;",
    "exec codex app-server --listen stdio:// 2>/tmp/codex-app-server.stderr",
  ].join(" ");
  return [shell, "-lc", command];
}

function codexSandboxEnv(
  modelBinding: ResolvedLlmBinding,
): Record<string, string> {
  const env: Record<string, string> = {
    ...pickEnv(
      (key) =>
        [
          "BRAINTRUST_API_KEY",
          "BRAINTRUST_APP_URL",
          "OPENAI_ORG_ID",
          "OPENAI_ORGANIZATION",
          "OPENAI_PROJECT",
        ].includes(key) || key.startsWith("CODEX_"),
    ),
    CODEX_HOME: "/tmp/exo-codex-home",
    HOME: "/tmp/exo-home",
  };
  if (modelBinding.apiKey) {
    env.OPENAI_API_KEY = modelBinding.apiKey;
  }
  if (modelBinding.baseUrl) {
    env.OPENAI_BASE_URL = modelBinding.baseUrl;
  }
  return env;
}

function buildCodexDynamicTools(context: TurnContext): JsonValue[] {
  if (useCodexExternalSandbox()) {
    return [];
  }
  if (!context.conversationConfig.shellProgram) {
    return [];
  }
  return [
    {
      name: CODEX_EXO_SHELL_DYNAMIC_TOOL,
      description: `Run a shell command through the exoharness sandbox. Commands execute from ${sandboxCwd(context)}. Use this for command execution in exo conversations.`,
      inputSchema: {
        type: "object",
        additionalProperties: false,
        properties: {
          command: {
            type: "string",
            description: "Shell command to execute.",
          },
        },
        required: ["command"],
      },
    },
  ];
}

function codexNativeSandboxPolicy(): JsonValue {
  if (useCodexExternalSandbox()) {
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

function useCodexExternalSandbox(): boolean {
  return true;
}

function codexAppServerCwd(context: TurnContext): string {
  return sandboxCwd(context);
}

function codexEffectiveSandboxProvider(
  context: TurnContext,
): TurnContext["agentConfig"]["sandboxProvider"] {
  return (
    context.conversationConfig.sandboxProvider ??
    context.agentConfig.sandboxProvider
  );
}

function codexEffectiveSandboxImage(context: TurnContext): string | null {
  return (
    context.conversationConfig.sandboxImage ??
    context.agentConfig.sandboxImage ??
    null
  );
}

function codexSandboxRuntimeKey(context: TurnContext): JsonValue {
  return {
    provider: codexEffectiveSandboxProvider(context),
    image: codexEffectiveSandboxImage(context),
    enable_networking: codexEffectiveNetworking(context),
    cwd: codexAppServerCwd(context),
    shell_program: context.conversationConfig.shellProgram ?? "/bin/bash",
    mounts: context.conversationConfig.mounts.map((mount) => ({
      host_path: mount.hostPath,
      mount_path: mount.mountPath,
      mode: mount.mode,
      internal: mount.internal ?? false,
    })),
    command: codexSandboxCommand(context),
    external_sandbox: useCodexExternalSandbox(),
  };
}

function codexWarmSessionKey(
  context: TurnContext,
  modelBinding: ResolvedLlmBinding,
  sandboxRuntimeKey: JsonValue,
): string {
  return JSON.stringify({
    agent_id: context.exoharness.current.agent.record.id,
    conversation_id: context.exoharness.current.conversation.record.id,
    model_binding: modelBinding.name,
    model: modelBinding.model,
    base_url: modelBinding.baseUrl ?? null,
    sandbox_runtime: sandboxRuntimeKey,
  });
}
