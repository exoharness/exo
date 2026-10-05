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
  type RawTurnRecord,
} from "../../typescript/harness/client";
import type {
  RawAgentConfig,
  RawConversationConfig,
  RawSendRequest,
} from "../../typescript/harness/wire";
import type { Message, ToolDefinition } from "../../typescript/harness/index";
import type { Env, SandboxIdentity, SandboxRequest } from "./env";
import { CloudflareSandbox } from "./sandbox";
import { Storage, type StorageOperation } from "./storage";
import type { Runtime } from "./runtime";

type Identity = { agent_id: string; thread_id: string };
type ProcessCommand = {
  argv: string[];
  env: Record<string, string>;
  cwd: string | null;
  timeout: { secs: number; nanos: number } | null;
};
type SandboxCommand =
  | {
      type: "acquire";
      request: SandboxRequest;
      snapshot: { id: string } | null;
    }
  | { type: "info" | "snapshot"; request: SandboxRequest }
  | { type: "stop"; request: SandboxRequest; terminate: boolean }
  | { type: "exec" | "start"; request: SandboxRequest; command: ProcessCommand }
  | { type: "read"; process_id: string; stream: "stdout" | "stderr" }
  | { type: "write"; process_id: string; bytes: number[] }
  | { type: "close_input" | "wait" | "close"; process_id: string };
type Process = Awaited<ReturnType<CloudflareSandbox["openProcess"]>>;
type RunningProcess = {
  process: Process;
  stdout: ReadableStreamDefaultReader<Uint8Array>;
  stderr: ReadableStreamDefaultReader<Uint8Array>;
  ended: Set<string>;
};
type HarnessRequest = Identity & {
  type: "harness";
  sandbox_id: string;
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
  | HarnessRequest;

export class RuntimeIO {
  private readonly codex = createCodexHarness(codexVersion.trim());
  private readonly storage: Storage;
  private readonly processes = new Map<string, RunningProcess>();
  constructor(
    private readonly ctx: DurableObjectState,
    private readonly env: Env,
    private readonly runtime: Runtime,
    private readonly emitProgress: (
      threadId: string,
      event: import("../../typescript/harness/client").RawEvent,
    ) => void,
  ) {
    this.storage = new Storage(ctx.storage, env.ARTIFACTS, ctx.id.toString());
  }

  async handle(request: HostRequest, signal: AbortSignal): Promise<unknown> {
    switch (request.type) {
      case "storage":
        return this.storage.handle(request.operation);
      case "sandbox":
        return this.sandbox(request.command);
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
      case "harness":
        return this.runHarness(request, signal);
    }
  }

  private async sandbox(command: SandboxCommand): Promise<unknown> {
    if ("process_id" in command) {
      const entry = this.processes.get(command.process_id);
      if (!entry) {
        if (command.type === "close" || command.type === "close_input")
          return null;
        throw new Error("sandbox process not found");
      }
      switch (command.type) {
        case "read": {
          let result: ReadableStreamReadResult<Uint8Array>;
          try {
            result = await entry[command.stream].read();
          } catch (error) {
            this.processes.delete(command.process_id);
            await entry.process.close();
            throw error;
          }
          const { done, value } = result;
          if (done) this.finished(command.process_id, entry, command.stream);
          return done ? null : [...value];
        }
        case "write":
          await entry.process.writeStdin(new Uint8Array(command.bytes));
          return null;
        case "close_input":
          await entry.process.closeStdin();
          return null;
        case "wait": {
          const code = await entry.process.wait();
          this.finished(command.process_id, entry, "wait");
          return code;
        }
        case "close":
          this.processes.delete(command.process_id);
          await entry.process.close();
          return null;
      }
    }
    const request = command.request;
    if (request.scope.type !== "thread")
      throw new Error("Cloudflare sandboxes require a thread scope");
    const identity: SandboxIdentity = {
      agentId: request.scope.agent_id,
      threadId: request.scope.thread_id,
      sandboxId: request.sandbox_id,
    };
    const stub = this.env.SANDBOXES.getByName(request.sandbox_id);
    switch (command.type) {
      case "acquire": {
        const environment = await this.runtime.call<Record<string, string>>({
          type: "sandbox_policy",
          request,
        });
        await stub.acquire(
          identity,
          request.spec.default_workdir,
          environment,
          command.snapshot,
          request.lifecycle.idle_ttl!.secs * 1000 +
            request.lifecycle.idle_ttl!.nanos / 1e6,
        );
        return null;
      }
      case "info":
        return stub.info(identity);
      case "snapshot":
        return { id: (await stub.snapshot(identity)).id };
      case "stop":
        if (command.terminate) await stub.terminate(identity);
        else await stub.stop();
        return null;
      case "exec": {
        const c = command.command;
        const result = await stub.exec(identity, {
          command: c.argv,
          env: c.env,
          cwd: c.cwd ?? undefined,
          timeoutMs: c.timeout
            ? c.timeout.secs * 1000 + c.timeout.nanos / 1e6
            : undefined,
        });
        return {
          ok: result.exitCode === 0,
          exit_code: result.exitCode,
          stdout: result.stdout,
          stderr: result.stderr,
          command: c.argv,
          cwd: c.cwd ?? request.spec.default_workdir,
        };
      }
      case "start": {
        const adapter = new CloudflareSandbox(stub, identity, (promise) =>
          this.ctx.waitUntil(promise),
        );
        const process = await adapter.openProcess({
          command: command.command.argv,
          env: command.command.env,
          cwd: command.command.cwd ?? undefined,
        });
        const id = crypto.randomUUID();
        this.processes.set(id, {
          process,
          stdout: process.stdout.getReader(),
          stderr: process.stderr.getReader(),
          ended: new Set(),
        });
        return id;
      }
    }
  }

  private finished(id: string, entry: RunningProcess, part: string): void {
    entry.ended.add(part);
    if (entry.ended.size === 3) {
      this.processes.delete(id);
      this.ctx.waitUntil(entry.process.close());
    }
  }

  private async runHarness(
    r: HarnessRequest,
    signal: AbortSignal,
  ): Promise<unknown> {
    const identity: SandboxIdentity = {
      agentId: r.agent_id,
      threadId: r.thread_id,
      sandboxId: r.sandbox_id,
    };
    const sandbox = new CloudflareSandbox(
      this.env.SANDBOXES.getByName(r.sandbox_id),
      identity,
      (promise) => this.ctx.waitUntil(promise),
    );
    const agent = await this.runtime.requestExo({
      type: "get_agent",
      agent_id: r.agent_id,
    });
    const thread = await this.runtime.requestExo({
      type: "get_conversation",
      agent_id: r.agent_id,
      conversation_id: r.thread_id,
    });
    if (
      agent.type !== "agent" ||
      !agent.agent ||
      thread.type !== "conversation" ||
      !thread.conversation
    )
      throw new Error("turn context disappeared");
    const client: HarnessClient = {
      requestExo: (request) => this.runtime.requestExo(request, signal),
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
        await sandbox.prepareCodex(codexVersion.trim());
        return sandbox.startProcess(request);
      },
      emitStream: async (
        event: Parameters<NonNullable<HarnessClient["emitStream"]>>[0],
      ) => {
        const progress = await this.runtime.call<
          import("../../typescript/harness/client").RawEvent | null
        >(
          { type: "progress", thread_id: r.thread_id, turn: r.turn, event },
          signal,
        );
        if (progress) this.emitProgress(r.thread_id, progress);
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
      this.ctx.waitUntil(this.env.SANDBOXES.getByName(r.sandbox_id).stop());
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
