import readline from "node:readline";
import { pathToFileURL } from "node:url";
import {
  validateToolPolicies,
  type SandboxProcess,
  type SandboxProcessStartRequest,
  type TurnContext,
  type TypeScriptHarness,
} from "./index";
import {
  createTurnContext,
  type RawTypeScriptInitPayload,
  type RawRuntimeRequest,
  type RawRuntimeResponsePayload,
  type RawRuntimeEvent,
  type RawSandboxProcessStream,
  type RawExoRequest,
  type RawExoResponse,
  type RawTypeScriptStreamEvent,
  type HostToGuestMessage,
  type GuestToHostMessage,
} from "./client";
class ProtocolClient {
  private nextRequestId = 1;
  private readonly pending = new Map<
    number,
    {
      resolve: (payload: unknown) => void;
      reject: (error: Error) => void;
    }
  >();
  private readonly initQueue: RawTypeScriptInitPayload[] = [];
  private readonly initWaiters: Array<{
    resolve: (payload: RawTypeScriptInitPayload | null) => void;
  }> = [];
  private readonly sandboxProcesses = new Map<number, SandboxProcessHandle>();
  private closed = false;

  constructor() {
    const rl = readline.createInterface({
      input: process.stdin,
      crlfDelay: Number.POSITIVE_INFINITY,
    });

    rl.on("line", (line) => {
      void this.handleLine(line).catch((error) => {
        void this.fail(error);
      });
    });

    rl.on("close", () => {
      const error = new Error(
        "typescript harness host pipe closed unexpectedly",
      );
      this.closeInitQueue();
      for (const pending of this.pending.values()) {
        pending.reject(error);
      }
      this.pending.clear();
    });
  }

  nextInit(): Promise<RawTypeScriptInitPayload | null> {
    const queued = this.initQueue.shift();
    if (queued) {
      return Promise.resolve(queued);
    }
    if (this.closed) {
      return Promise.resolve(null);
    }
    return new Promise<RawTypeScriptInitPayload | null>((resolve) => {
      this.initWaiters.push({ resolve });
    });
  }

  async requestRuntime(
    request: RawRuntimeRequest,
  ): Promise<RawRuntimeResponsePayload> {
    const id = this.nextRequestId;
    this.nextRequestId += 1;
    const response = new Promise<RawRuntimeResponsePayload>(
      (resolve, reject) => {
        this.pending.set(id, {
          resolve: (payload: unknown) =>
            resolve(payload as RawRuntimeResponsePayload),
          reject,
        });
      },
    );
    await this.send({
      kind: "runtime_request",
      id,
      request,
    });
    return response;
  }

  async requestExo(request: RawExoRequest): Promise<RawExoResponse> {
    const id = this.nextRequestId;
    this.nextRequestId += 1;
    const response = new Promise<RawExoResponse>((resolve, reject) => {
      this.pending.set(id, {
        resolve: (payload: unknown) => resolve(payload as RawExoResponse),
        reject,
      });
    });
    await this.send({
      kind: "exo_request",
      id,
      request,
    });
    try {
      return await response;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      throw new Error(`exoharness request ${request.type} failed: ${message}`);
    }
  }

  async emitStream(event: RawTypeScriptStreamEvent): Promise<void> {
    await this.send({
      kind: "stream_event",
      event,
    });
  }

  async startSandboxProcess(
    request: SandboxProcessStartRequest,
  ): Promise<SandboxProcess> {
    const payload = await this.requestRuntime({
      type: "start_sandbox_process",
      command: request.command,
      env: request.env ?? {},
      reuse_key: request.reuseKey ?? null,
    });
    if (payload.type !== "sandbox_process_started") {
      throw new Error(
        `expected sandbox_process_started payload, got ${payload.type}`,
      );
    }
    const process = new SandboxProcessHandle(
      this,
      payload.process_id,
      payload.sandbox_id ?? undefined,
      payload.sandbox_process_id ?? undefined,
      payload.reused === true,
    );
    this.sandboxProcesses.set(payload.process_id, process);
    return process;
  }

  async writeSandboxProcessStdin(
    processId: number,
    data: string,
  ): Promise<void> {
    const payload = await this.requestRuntime({
      type: "write_sandbox_process_stdin",
      process_id: processId,
      data,
    });
    if (payload.type !== "unit") {
      throw new Error(`expected unit payload, got ${payload.type}`);
    }
  }

  async closeSandboxProcessStdin(processId: number): Promise<void> {
    const payload = await this.requestRuntime({
      type: "close_sandbox_process_stdin",
      process_id: processId,
    });
    if (payload.type !== "unit") {
      throw new Error(`expected unit payload, got ${payload.type}`);
    }
  }

  async closeSandboxProcess(processId: number): Promise<void> {
    const payload = await this.requestRuntime({
      type: "close_sandbox_process",
      process_id: processId,
    });
    if (payload.type !== "unit") {
      throw new Error(`expected unit payload, got ${payload.type}`);
    }
  }

  async done(): Promise<void> {
    await this.send({ kind: "done" });
  }

  async fail(error: unknown): Promise<void> {
    const message = error instanceof Error ? error.message : String(error);
    const stack = error instanceof Error ? (error.stack ?? null) : null;
    await this.send({
      kind: "error",
      message,
      stack,
    });
  }

  private async handleLine(line: string): Promise<void> {
    const message = JSON.parse(line) as HostToGuestMessage;
    switch (message.kind) {
      case "init":
        this.enqueueInit(message.payload);
        return;
      case "shutdown":
        this.closeInitQueue();
        return;
      case "runtime_response": {
        const pending = this.pending.get(message.id);
        if (!pending) {
          throw new Error(`unexpected runtime response id ${message.id}`);
        }
        this.pending.delete(message.id);
        if (!message.ok) {
          pending.reject(
            new Error(
              message.error ?? "typescript harness runtime request failed",
            ),
          );
          return;
        }
        if (!message.payload) {
          pending.reject(
            new Error(
              `missing runtime response payload for request ${message.id}`,
            ),
          );
          return;
        }
        pending.resolve(message.payload);
        return;
      }
      case "exo_response": {
        const pending = this.pending.get(message.id);
        if (!pending) {
          throw new Error(`unexpected exoharness response id ${message.id}`);
        }
        this.pending.delete(message.id);
        if (!message.ok) {
          pending.reject(
            new Error(message.error ?? "exoharness request failed"),
          );
          return;
        }
        if (!message.response) {
          pending.reject(
            new Error(
              `missing exoharness response payload for request ${message.id}`,
            ),
          );
          return;
        }
        pending.resolve(message.response);
        return;
      }
      case "runtime_event":
        this.handleRuntimeEvent(message.event);
        return;
    }
  }

  private async send(message: GuestToHostMessage): Promise<void> {
    process.stdout.write(`${JSON.stringify(message)}\n`);
  }

  private enqueueInit(payload: RawTypeScriptInitPayload): void {
    const waiter = this.initWaiters.shift();
    if (waiter) {
      waiter.resolve(payload);
      return;
    }
    this.initQueue.push(payload);
  }

  private closeInitQueue(): void {
    this.closed = true;
    while (this.initWaiters.length > 0) {
      const waiter = this.initWaiters.shift();
      waiter?.resolve(null);
    }
  }

  private handleRuntimeEvent(event: RawRuntimeEvent): void {
    const process = this.sandboxProcesses.get(event.process_id);
    if (!process) {
      return;
    }
    process.handleEvent(event);
    if (
      event.type === "sandbox_process_exit" ||
      event.type === "sandbox_process_error"
    ) {
      this.sandboxProcesses.delete(event.process_id);
    }
  }
}

class SandboxProcessHandle implements SandboxProcess {
  readonly reused: boolean;
  readonly stdout: ReadableStream<string>;
  readonly stderr: ReadableStream<string>;
  private stdoutController: ReadableStreamDefaultController<string> | null =
    null;
  private stderrController: ReadableStreamDefaultController<string> | null =
    null;
  private finished = false;
  private readonly waitPromise: Promise<number | null>;
  private resolveWait!: (exitCode: number | null) => void;
  private rejectWait!: (error: Error) => void;

  constructor(
    private readonly client: ProtocolClient,
    private readonly processId: number,
    readonly sandboxId?: string,
    readonly sandboxProcessId?: string,
    reused = false,
  ) {
    this.reused = reused;
    this.stdout = new ReadableStream<string>({
      start: (controller) => {
        this.stdoutController = controller;
      },
    });
    this.stderr = new ReadableStream<string>({
      start: (controller) => {
        this.stderrController = controller;
      },
    });
    this.waitPromise = new Promise<number | null>((resolve, reject) => {
      this.resolveWait = resolve;
      this.rejectWait = reject;
    });
  }

  async writeStdin(data: string): Promise<void> {
    await this.client.writeSandboxProcessStdin(this.processId, data);
  }

  async closeStdin(): Promise<void> {
    await this.client.closeSandboxProcessStdin(this.processId);
  }

  async close(): Promise<void> {
    if (this.finished) {
      return;
    }
    await this.client.closeSandboxProcess(this.processId);
  }

  wait(): Promise<number | null> {
    return this.waitPromise;
  }

  handleEvent(event: RawRuntimeEvent): void {
    switch (event.type) {
      case "sandbox_process_output":
        this.enqueue(event.stream, event.data);
        return;
      case "sandbox_process_exit":
        this.finish(event.exit_code ?? null);
        return;
      case "sandbox_process_error":
        this.fail(new Error(event.message));
        return;
    }
  }

  private enqueue(stream: RawSandboxProcessStream, data: string): void {
    const controller =
      stream === "stdout" ? this.stdoutController : this.stderrController;
    controller?.enqueue(data);
  }

  private finish(exitCode: number | null): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    this.stdoutController?.close();
    this.stderrController?.close();
    this.resolveWait(exitCode);
  }

  private fail(error: Error): void {
    if (this.finished) {
      return;
    }
    this.finished = true;
    this.stdoutController?.error(error);
    this.stderrController?.error(error);
    this.rejectWait(error);
  }
}

function resolveHarnessModule(
  moduleExports: Record<string, unknown>,
): TypeScriptHarness {
  const candidate = moduleExports.default ?? moduleExports.harness;
  if (!candidate || typeof candidate !== "object") {
    throw new Error(
      "typescript harness module must export a default harness or a named `harness` export",
    );
  }
  if (!("runTurn" in candidate) || typeof candidate.runTurn !== "function") {
    throw new Error(
      "typescript harness export must have an async runTurn(context) method",
    );
  }
  return candidate as TypeScriptHarness;
}

async function hasUnresolvedToolCalls(context: TurnContext): Promise<boolean> {
  const pending = new Map<string, number>();
  let cursor: string | null = null;
  do {
    const page = await context.exoharness.current.conversation.getEvents({
      cursor,
      direction: "asc",
      limit: 100,
      turnId: context.exoharness.current.turn.record.id,
      types: ["tool_requested", "tool_result"],
    });
    for (const event of page.events) {
      const id = event.data.tool_call_id;
      if (typeof id !== "string") continue;
      if (event.data.type === "tool_requested") {
        pending.set(id, (pending.get(id) ?? 0) + 1);
      } else if (event.data.type === "tool_result") {
        const count = pending.get(id) ?? 0;
        if (count <= 1) pending.delete(id);
        else pending.set(id, count - 1);
      }
    }
    cursor = page.cursor ?? null;
  } while (cursor);
  return pending.size > 0;
}

async function main(): Promise<void> {
  const client = new ProtocolClient();
  const modulePath = process.argv[2];
  if (!modulePath) {
    throw new Error("missing harness module path");
  }
  let harness: TypeScriptHarness;
  try {
    const moduleExports = (await import(
      pathToFileURL(modulePath).href
    )) as Record<string, unknown>;
    harness = resolveHarnessModule(moduleExports);
  } catch (error) {
    await client.fail(error);
    throw error;
  }

  for (;;) {
    const init = await client.nextInit();
    if (!init) {
      return;
    }
    const context = createTurnContext(client, init);
    try {
      if (harness.nativeToolApprovals !== true) {
        validateToolPolicies(
          context,
          [
            ...context.tools.map((tool) => tool.name),
            ...(context.conversationConfig.shellProgram ? ["shell"] : []),
          ],
          false,
        );
      }
      if (init.recovering) {
        if (!harness.resumeTurn) {
          throw new Error(
            "this TypeScript harness cannot safely resume an unfinished turn",
          );
        }
        if (
          harness.reconcileUnresolvedToolCalls !== true &&
          (await hasUnresolvedToolCalls(context))
        ) {
          throw new Error(
            "this TypeScript harness cannot safely resume an unresolved tool call",
          );
        }
        await harness.resumeTurn(context);
      } else {
        await harness.runTurn(context);
      }
      await client.done();
    } catch (error) {
      await client.fail(error);
      throw error;
    }
  }
}

void main().catch(() => {
  process.exitCode = 1;
});
