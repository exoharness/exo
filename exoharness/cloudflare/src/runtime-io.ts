import { init as initLingua } from "@braintrust/lingua/browser";
import linguaWasm from "@braintrust/lingua-wasm/browser/lingua_bg.wasm";
import { createCodexHarness } from "../../typescript/codex/harness";
import codexVersion from "../../containers/codex-sandbox/version";
import {
  ResponsesRuntime,
  responseMessages,
  responseToolCalls,
} from "../../typescript/model-runtime/responses";
import {
  toAgentConfig,
  toConversationConfig,
  toSendRequest,
  type RawAgentConfig,
  type RawConversationConfig,
  type RawSendRequest,
} from "../../typescript/harness/wire";
import type {
  Conversation,
  Message,
  ToolDefinition,
  TurnContext,
} from "../../typescript/harness/core";
import { fields, text } from "./definition";
import type { Env, SandboxIdentity, SandboxPolicy } from "./env";
import { PLACEHOLDER_PREFIX } from "./network";
import { CloudflareSandbox } from "./sandbox";
import { CloudflareExoHarness } from "./state";
import { stateRequest, type StateRequest } from "./state-protocol";

interface ModelRequest {
  model: string;
  api_key: string;
  base_url: string | null;
  messages: Message[];
  tools: ToolDefinition[];
  max_output_tokens: number | null;
}
type Identity = { agent_id: string; thread_id: string };
type HarnessRequest = Identity & {
  type: "harness";
  turn: { id: string; session_id: string };
  agent_config: RawAgentConfig;
  conversation_config: RawConversationConfig;
  request: RawSendRequest;
  recovering: boolean;
};
export type HostRequest =
  | { type: "state"; request: StateRequest }
  | (Identity & { type: "watch"; after: string | null })
  | { type: "model"; request: ModelRequest }
  | (Identity & {
      type: "tool";
      request: { function_name: string; arguments: unknown };
    })
  | HarnessRequest
  | { type: "stop_sandbox"; thread_id: string };

export class RuntimeIO {
  private readonly codex = createCodexHarness(codexVersion.trim(), {
    reuseSessions: false,
    sandboxEnv: { HOME: "/home/exo", CODEX_HOME: "/home/exo/.codex" },
  });
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly env: Env,
    private readonly harness: CloudflareExoHarness,
    private readonly watch: (
      request: Extract<HostRequest, { type: "watch" }>,
      signal: AbortSignal,
    ) => Promise<unknown>,
    private readonly credential: (
      context: Conversation,
      name: string,
      url: string,
    ) => Promise<string>,
  ) {}

  async handle(request: HostRequest, signal: AbortSignal): Promise<unknown> {
    switch (request.type) {
      case "state":
        return stateRequest(this.harness, this.ctx, request.request);
      case "watch":
        return this.watch(request, signal);
      case "stop_sandbox":
        await this.env.SANDBOXES.getByName(request.thread_id).stop();
        return {};
      case "model": {
        const r = request.request;
        await initLingua(linguaWasm);
        const response = await new ResponsesRuntime({
          apiKey: r.api_key,
          baseURL: r.base_url ?? undefined,
        }).complete({
          model: r.model,
          messages: r.messages,
          tools: r.tools,
          maxOutputTokens: r.max_output_tokens,
        });
        return {
          response_id: null,
          messages: responseMessages(response),
          model: response.model,
          tool_calls: responseToolCalls(response).map((call) => ({
            tool_call_id: call.toolCallId,
            request: {
              function_name: call.request.functionName,
              arguments: call.request.arguments,
            },
          })),
          usage: response.usage
            ? {
                prompt_tokens: response.usage.input_tokens,
                completion_tokens: response.usage.output_tokens,
                prompt_cached_tokens:
                  response.usage.input_tokens_details?.cached_tokens,
                completion_reasoning_tokens:
                  response.usage.output_tokens_details?.reasoning_tokens,
              }
            : null,
        };
      }
      case "tool": {
        signal.throwIfAborted();
        if (request.request.function_name !== "shell")
          throw new Error("unsupported Worker tool");
        const args = fields(request.request.arguments, ["command"]);
        const output = await this.env.SANDBOXES.getByName(
          request.thread_id,
        ).exec(
          { agentId: request.agent_id, threadId: request.thread_id },
          {
            command: ["/bin/bash", "-lc", text(args.command, "shell command")],
          },
        );
        return { result: output };
      }
      case "harness":
        return this.runHarness(request, signal);
    }
  }

  private async runHarness(
    r: HarnessRequest,
    signal: AbortSignal,
  ): Promise<unknown> {
    const identity: SandboxIdentity = {
      agentId: r.agent_id,
      threadId: r.thread_id,
    };
    const harness = await this.harness.forTurn(r.agent_id, r.thread_id, {
      id: r.turn.id,
      sessionId: r.turn.session_id,
    });
    const agentConfig = toAgentConfig(r.agent_config);
    const conversationConfig = toConversationConfig(r.conversation_config);
    const baseURL = agentConfig.baseUrl ?? "https://api.openai.com/v1";
    const credential = text(agentConfig.credential, "credential");
    await this.credential(
      harness.current.conversation,
      credential,
      `${baseURL.replace(/\/$/, "")}/responses`,
    );
    const saved = this.harness.store.get<SandboxPolicy>(
      `codex-policy/${r.thread_id}`,
    );
    this.harness.store.put(`codex-policy/${r.thread_id}`, {
      origins: [new URL(baseURL).origin],
      credentials: [
        {
          environmentVariable: "OPENAI_API_KEY",
          credential,
          placeholder:
            saved?.credentials[0]?.credential === credential
              ? saved.credentials[0].placeholder
              : `${PLACEHOLDER_PREFIX}${crypto.randomUUID()}`,
        },
      ],
    } satisfies SandboxPolicy);
    const append = (
      event_type: string,
      payload: { text?: string; ttft_ms?: number; duration_ms?: number },
    ) =>
      harness.current.turn
        .addEvents([{ type: "custom", event_type, payload }])
        .then(() => {});
    const sandbox = new CloudflareSandbox(
      this.env.SANDBOXES.getByName(r.thread_id),
      identity,
      (promise) => this.ctx.waitUntil(promise),
    );
    const context: TurnContext = {
      exoharness: harness,
      agentConfig,
      conversationConfig,
      request: toSendRequest(r.request),
      mcpServers: [],
      tools: [],
      streaming: true,
      authorizeTool: async () => {},
      executeTool: async () => {
        throw new Error("custom Codex tools are not supported");
      },
      executePendingTools: async () => {
        throw new Error("custom Codex tools are not supported");
      },
      startSandboxProcess: async (request) => {
        await append("codex_process_start_requested", {});
        await sandbox.prepareCodex(codexVersion.trim());
        const process = await sandbox.startProcess(request);
        await append("codex_process_started", {});
        return {
          ...process,
          close: async () => {
            const startedAt = Date.now();
            await process.close();
            await append("codex_process_closed", {
              duration_ms: Date.now() - startedAt,
            });
          },
        };
      },
      stream: {
        firstChunk: (ttft_ms) => append("codex_first_chunk", { ttft_ms }),
        text: (text) => append("codex_text_delta", { text }),
        toolCall: async () => {},
        toolResult: async () => {},
      },
    };
    const stop = () =>
      this.ctx.waitUntil(this.env.SANDBOXES.getByName(r.thread_id).stop());
    signal.throwIfAborted();
    signal.addEventListener("abort", stop, { once: true });
    const timer = setTimeout(stop, 600_000);
    try {
      await initLingua(linguaWasm);
      if (r.recovering) await this.codex.resumeTurn!(context);
      else await this.codex.runTurn(context);
      signal.throwIfAborted();
      // The harness closes Codex and flushes its history before this snapshot.
      const snapshotStartedAt = Date.now();
      await this.env.SANDBOXES.getByName(r.thread_id).snapshot(identity);
      await append("codex_snapshot_completed", {
        duration_ms: Date.now() - snapshotStartedAt,
      });
      return {};
    } finally {
      clearTimeout(timer);
      signal.removeEventListener("abort", stop);
    }
  }
}
