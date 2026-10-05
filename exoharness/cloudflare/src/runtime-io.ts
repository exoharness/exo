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
  type RawTypeScriptInitPayload,
} from "../../typescript/harness/client";
import type { Message, ToolDefinition } from "../../typescript/harness/index";
import { SandboxProcessClient } from "../../typescript/harness/sandbox-process";
import type { Env, SandboxIdentity, SandboxRequest } from "./env";
import { CloudflareSandbox } from "./sandbox";
import { Storage, type StorageOperation } from "./storage";
import type { Runtime } from "./runtime";

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
  | {
      type: "begin_activity" | "end_activity";
      request: SandboxRequest;
      id: string;
    }
  | { type: "stop"; request: SandboxRequest; terminate: boolean }
  | { type: "exec" | "start"; request: SandboxRequest; command: ProcessCommand }
  | { type: "read"; process: RunningProcess; stream: "stdout" | "stderr" }
  | { type: "write"; process: RunningProcess; data: Uint8Array }
  | { type: "close_input" | "wait" | "close"; process: RunningProcess };
type Process = Awaited<ReturnType<CloudflareSandbox["openProcess"]>>;
type RunningProcess = {
  process: Process;
  stdout: ReadableStreamDefaultReader<Uint8Array>;
  stderr: ReadableStreamDefaultReader<Uint8Array>;
  ended: Set<string>;
};
type HarnessRequest = {
  type: "harness";
  payload: RawTypeScriptInitPayload;
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
  readonly harnessProcesses = new SandboxProcessClient();
  private readonly codex = createCodexHarness(codexVersion.trim());
  private readonly storage: Storage;
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
        return this.sandbox(request.command, signal);
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

  private async sandbox(
    command: SandboxCommand,
    signal: AbortSignal,
  ): Promise<unknown> {
    if ("process" in command) {
      const entry = command.process;
      switch (command.type) {
        case "read": {
          let result: ReadableStreamReadResult<Uint8Array>;
          try {
            result = await entry[command.stream].read();
          } catch (error) {
            this.finished(entry, command.stream);
            await entry.process.close();
            throw error;
          }
          const { done, value } = result;
          if (done) this.finished(entry, command.stream);
          return done ? null : value;
        }
        case "write":
          await entry.process.writeStdin(command.data);
          return null;
        case "close_input":
          await entry.process.closeStdin();
          return null;
        case "wait": {
          const code = await entry.process.wait();
          this.finished(entry, "wait");
          return code;
        }
        case "close":
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
          codexVersion.trim(),
        );
        return null;
      }
      case "begin_activity":
        await stub.beginActivity(identity, command.id);
        if (signal.aborted) {
          await stub.endActivity(command.id);
          signal.throwIfAborted();
        }
        return null;
      case "end_activity":
        await stub.endActivity(command.id);
        // Refresh the invocation keepalive after each turn so a long
        // conversation can still use the full sandbox idle window.
        this.ctx.waitUntil(stub.waitForProcesses());
        return null;
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
        return {
          process,
          stdout: process.stdout.getReader(),
          stderr: process.stderr.getReader(),
          ended: new Set<string>(),
        };
      }
    }
  }

  private finished(entry: RunningProcess, part: string): void {
    entry.ended.add(part);
    if (entry.ended.size === 3) {
      this.ctx.waitUntil(entry.process.close());
    }
  }

  private async runHarness(
    r: HarnessRequest,
    signal: AbortSignal,
  ): Promise<unknown> {
    const { payload } = r;
    const threadId = payload.conversation.record.id;
    const client: HarnessClient = {
      requestExo: (request) => this.runtime.requestExo(request, signal),
      requestRuntime: (request) =>
        this.runtime.call(
          { type: "harness_request", thread_id: threadId, request },
          // Warm process handles retain this client across turns. Their I/O
          // and cleanup must survive this turn's abort; other requests use it.
          request.type === "write_sandbox_process_stdin" ||
            request.type === "close_sandbox_process_stdin" ||
            request.type === "close_sandbox_process"
            ? undefined
            : signal,
        ),
      startSandboxProcess: (request) =>
        this.harnessProcesses.start(request, client.requestRuntime),
      emitStream: async (event) => {
        const progress = await this.runtime.call<
          import("../../typescript/harness/client").RawEvent | null
        >(
          {
            type: "progress",
            thread_id: threadId,
            turn: payload.turn.record,
            event,
          },
          signal,
        );
        if (progress) this.emitProgress(threadId, progress);
      },
    };
    const context = createTurnContext(client, payload);
    signal.throwIfAborted();
    await initLingua(linguaWasm);
    if (payload.recovering) await this.codex.resumeTurn!(context);
    else await this.codex.runTurn(context);
    signal.throwIfAborted();
    return null;
  }
}
