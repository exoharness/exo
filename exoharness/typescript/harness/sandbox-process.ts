import type { SandboxProcess } from "./index";
import type { RawSandboxProcessStream } from "./client";

type ProcessEvent =
  | {
      type: "sandbox_process_output";
      stream: RawSandboxProcessStream;
      data: string;
    }
  | { type: "sandbox_process_exit"; exit_code?: number | null }
  | { type: "sandbox_process_error"; message: string };

export class SandboxProcessHandle implements SandboxProcess {
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
