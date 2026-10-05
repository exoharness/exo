import { initSync, WorkerRuntime } from "./wasm/exo_worker_runtime";
import runtimeWasm from "./wasm/exo_worker_runtime_bg.wasm";
import type { HostRequest } from "./runtime-io";

initSync({ module: runtimeWasm });

interface Progress {
  calls: { id: number; request: HostRequest }[];
  cancelled: number[];
  completed: { id: number; result: string | null; error: string | null }[];
  pending: boolean;
}

/** Owns the portable Rust runtime; JavaScript supplies asynchronous host I/O. */
export class Runtime {
  private readonly wasm = new WorkerRuntime();
  private readonly operations = new Map<
    number,
    { resolve(value: unknown): void; reject(error: Error): void }
  >();
  private readonly calls = new Map<number, AbortController>();
  private idle?: { promise: Promise<void>; resolve(): void };

  constructor(
    private readonly handle: (
      request: HostRequest,
      signal: AbortSignal,
    ) => Promise<unknown>,
    private readonly waitUntil: (promise: Promise<unknown>) => void,
  ) {}

  call<T>(operation: unknown): Promise<T> {
    const id = this.wasm.submit(JSON.stringify(operation));
    const result = new Promise<T>((resolve, reject) => {
      this.operations.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
      });
    });
    this.pump();
    return result;
  }

  private pump(): void {
    const progress: Progress = JSON.parse(this.wasm.poll());
    for (const id of progress.cancelled) this.calls.get(id)?.abort();
    for (const completion of progress.completed) {
      const operation = this.operations.get(completion.id)!;
      this.operations.delete(completion.id);
      if (completion.error !== null)
        operation.reject(new Error(completion.error));
      else operation.resolve(JSON.parse(completion.result!));
    }
    if (progress.pending && !this.idle) {
      let resolve!: () => void;
      const promise = new Promise<void>((done) => {
        resolve = done;
      });
      this.idle = { promise, resolve };
      this.waitUntil(promise);
    } else if (!progress.pending && this.idle) {
      this.idle.resolve();
      this.idle = undefined;
    }
    for (const call of progress.calls) {
      const controller = new AbortController();
      this.calls.set(call.id, controller);
      const respond = async () => {
        let reply: { result: string } | { error: string };
        try {
          const value = await this.handle(call.request, controller.signal);
          reply = { result: JSON.stringify(value) };
        } catch (error) {
          reply = {
            error: error instanceof Error ? error.message : String(error),
          };
        }
        this.calls.delete(call.id);
        this.wasm.resolve(call.id, JSON.stringify(reply));
        this.pump();
      };
      this.waitUntil(respond());
    }
  }
}
