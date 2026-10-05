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
  createTurnContext,
  type HarnessClient,
  type RawAgentRecord,
  type RawConversationHandleInfo,
  type RawExoResponse,
  type RawTurnRecord,
} from "../../typescript/harness/client";
import type {
  RawAgentConfig,
  RawConversationConfig,
  RawSendRequest,
} from "../../typescript/harness/wire";
import type { Message, ToolDefinition } from "../../typescript/harness/index";
import type { Env, SandboxIdentity } from "./env";
import { CloudflareSandbox } from "./sandbox";
import { Storage, type StorageOperation } from "./storage";
import type { Runtime } from "./runtime";

type Identity = { agent_id: string; thread_id: string };
type SandboxCommand = Identity &
  ({ type: "info" | "snapshot" } | { type: "stop"; terminate: boolean });
type HarnessRequest = Identity & {
  type: "harness";
  turn: RawTurnRecord;
  agent_config: RawAgentConfig;
  conversation_config: RawConversationConfig;
  request: RawSendRequest;
  recovering: boolean;
};
export type HostRequest =
  | { type: "storage"; operation: StorageOperation }
  | { type: "sandbox"; command: SandboxCommand }
  | {
      type: "model";
      request: {
        model: string;
        api_key: string;
        base_url: string | null;
        messages: Message[];
        tools: ToolDefinition[];
        max_output_tokens: number | null;
      };
    }
  | (Identity & {
      type: "exec";
      command: string[];
    })
  | HarnessRequest
  | { type: "stop_sandbox"; thread_id: string };

export class RuntimeIO {
  private readonly codex = createCodexHarness(codexVersion.trim());
  private readonly storage: Storage;
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly env: Env,
    private readonly runtime: Runtime,
  ) {
    this.storage = new Storage(ctx.storage, env.ARTIFACTS, ctx.id.toString());
  }

  async handle(request: HostRequest, signal: AbortSignal): Promise<unknown> {
    switch (request.type) {
      case "storage":
        return this.storage.handle(request.operation);
      case "sandbox": {
        const command = request.command;
        const sandbox = this.env.SANDBOXES.getByName(command.thread_id);
        const identity = {
          agentId: command.agent_id,
          threadId: command.thread_id,
        };
        if (command.type === "info") return sandbox.info(identity);
        if (command.type === "snapshot")
          return (await sandbox.snapshot(identity)).id;
        if (command.type === "stop") {
          if (command.terminate) await sandbox.terminate(identity);
          else await sandbox.stop();
          return null;
        }
        throw new Error("unsupported sandbox command");
      }
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
      case "exec": {
        signal.throwIfAborted();
        return this.env.SANDBOXES.getByName(request.thread_id).exec(
          { agentId: request.agent_id, threadId: request.thread_id },
          { command: request.command },
        );
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
    const sandbox = new CloudflareSandbox(
      this.env.SANDBOXES.getByName(r.thread_id),
      identity,
      (promise) => this.ctx.waitUntil(promise),
    );
    const agent = await this.runtime.call<RawExoResponse>({
      type: "state",
      request: { type: "get_agent", agent_id: r.agent_id },
    });
    const thread = await this.runtime.call<RawExoResponse>({
      type: "state",
      request: {
        type: "get_conversation",
        agent_id: r.agent_id,
        conversation_id: r.thread_id,
      },
    });
    if (
      agent.type !== "agent" ||
      !agent.agent ||
      thread.type !== "conversation" ||
      !thread.conversation
    )
      throw new Error("turn context disappeared");
    const client: HarnessClient = {
      requestExo: (request) =>
        this.runtime.call({ type: "state", request }, signal),
      requestRuntime: (request) => {
        if (request.type !== "authorize_tool")
          throw new Error("custom Codex tools are not supported");
        return this.runtime.call(
          {
            type: "authorize_tool",
            agent_id: r.agent_id,
            thread_id: r.thread_id,
            turn: r.turn,
            request: request.request,
          },
          signal,
        );
      },
      startSandboxProcess: async (request) => {
        await context.exoharness.current.turn.addEvents([
          {
            type: "custom",
            event_type: "codex_process_start_requested",
            payload: {},
          },
        ]);
        await sandbox.prepareCodex(codexVersion.trim());
        const process = await sandbox.startProcess(request);
        await context.exoharness.current.turn.addEvents([
          { type: "custom", event_type: "codex_process_started", payload: {} },
        ]);
        return process;
      },
      emitStream: async (event) => {
        await context.exoharness.current.turn.addEvents([
          { type: "custom", event_type: `codex_${event.type}`, payload: event },
        ]);
      },
    };
    const context = createTurnContext(client, {
      agent: agent.agent as RawAgentRecord,
      conversation: thread.conversation as RawConversationHandleInfo,
      turn: { conversation: thread.conversation, record: r.turn },
      agent_config: r.agent_config,
      conversation_config: r.conversation_config,
      request: r.request,
      streaming: true,
      recovering: r.recovering,
      mcp_servers: [],
      tools: [],
    });
    const stop = () =>
      this.ctx.waitUntil(this.env.SANDBOXES.getByName(r.thread_id).stop());
    signal.throwIfAborted();
    signal.addEventListener("abort", stop, { once: true });
    const timer = setTimeout(stop, 600_000);
    try {
      return await sandbox.runTurn(r.turn.id, async () => {
        await initLingua(linguaWasm);
        if (r.recovering) await this.codex.resumeTurn!(context);
        else await this.codex.runTurn(context);
        signal.throwIfAborted();
        return {};
      });
    } finally {
      clearTimeout(timer);
      signal.removeEventListener("abort", stop);
    }
  }
}
