import type { SandboxProcess, SandboxProcessStartRequest } from "./index";
import type {
  RawSandboxProcessStream,
  RawRuntimeRequest,
  RawRuntimeResponsePayload,
  RawRuntimeEvent,
} from "./client";

type ProcessEvent =
  | {
      type: "sandbox_process_output";
      stream: RawSandboxProcessStream;
      data: string;
    }
  | { type: "sandbox_process_exit"; exit_code?: number | null }
  | { type: "sandbox_process_error"; message: string };

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

  readonly sandboxId?: string;
  readonly sandboxProcessId?: string;

  constructor(
    private readonly control: Pick<
      SandboxProcess,
      "writeStdin" | "closeStdin" | "close"
    >,
    identity: Pick<SandboxProcess, "sandboxId" | "sandboxProcessId" | "reused">,
  ) {
    this.sandboxId = identity.sandboxId;
    this.sandboxProcessId = identity.sandboxProcessId;
    this.reused = identity.reused;
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
    await this.control.writeStdin(data);
  }

  async closeStdin(): Promise<void> {
    await this.control.closeStdin();
  }

  async close(): Promise<void> {
    if (this.finished) {
      return;
    }
    await this.control.close();
  }

  wait(): Promise<number | null> {
    return this.waitPromise;
  }

  handleEvent(event: ProcessEvent): void {
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

export class SandboxProcessClient {
  private readonly sandboxProcesses = new Map<
    number,
    SandboxProcessHandle | RawRuntimeEvent[]
  >();
  private pendingStarts = 0;
  async start(
    request: SandboxProcessStartRequest,
    requestRuntime: (
      request: RawRuntimeRequest,
    ) => Promise<RawRuntimeResponsePayload>,
  ): Promise<SandboxProcess> {
    this.pendingStarts++;
    try {
      const payload = await requestRuntime({
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
      const unit = async (request: RawRuntimeRequest) => {
        const response = await requestRuntime(request);
        if (response.type !== "unit")
          throw new Error(`expected unit payload, got ${response.type}`);
      };
      const process = new SandboxProcessHandle(
        {
          writeStdin: (data) =>
            unit({
              type: "write_sandbox_process_stdin",
              process_id: payload.process_id,
              data,
            }),
          closeStdin: () =>
            unit({
              type: "close_sandbox_process_stdin",
              process_id: payload.process_id,
            }),
          close: () =>
            unit({
              type: "close_sandbox_process",
              process_id: payload.process_id,
            }),
        },
        {
          sandboxId: payload.sandbox_id ?? undefined,
          sandboxProcessId: payload.sandbox_process_id ?? undefined,
          reused: payload.reused === true,
        },
      );
      const queued = this.sandboxProcesses.get(payload.process_id);
      this.sandboxProcesses.set(payload.process_id, process);
      if (Array.isArray(queued))
        for (const event of queued) this.handleEvent(event);
      return process;
    } finally {
      this.pendingStarts--;
      if (this.pendingStarts === 0)
        for (const [id, value] of this.sandboxProcesses)
          if (Array.isArray(value)) this.sandboxProcesses.delete(id);
    }
  }

  handleEvent(event: RawRuntimeEvent): void {
    const process = this.sandboxProcesses.get(event.process_id);
    if (!process || Array.isArray(process)) {
      if (this.pendingStarts > 0) {
        const queued = process ?? [];
        queued.push(event);
        this.sandboxProcesses.set(event.process_id, queued);
      }
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
