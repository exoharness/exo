import { initSync, WorkerRuntime } from "./wasm/exo_worker_runtime";
import runtimeWasm from "./wasm/exo_worker_runtime_bg.wasm";
import type { HostRequest } from "./runtime-io";
import type {
  RawExoRequest,
  RawExoResponse,
  RawRuntimeEvent,
} from "../../typescript/harness/client";

initSync({ module: runtimeWasm });

interface Progress {
  calls: { id: number; request: HostRequest }[];
  cancelled: number[];
  completed: { id: number; result: unknown; error: string | null }[];
  pending: boolean;
  events: RawRuntimeEvent[];
}

/** Owns the portable Rust runtime; JavaScript supplies asynchronous host I/O. */
export class Runtime {
  private readonly wasm: WorkerRuntime;
  private readonly operations = new Map<
    number,
    { resolve(value: unknown): void; reject(error: Error): void }
  >();
  private readonly calls = new Map<number, AbortController>();
  private idle?: { promise: Promise<void>; resolve(): void };

  constructor(
    masterKey: string,
    private readonly handle: (
      request: HostRequest,
      signal: AbortSignal,
    ) => Promise<unknown>,
    private readonly waitUntil: (promise: Promise<unknown>) => void,
    private readonly emitRuntimeEvent: (event: RawRuntimeEvent) => void,
  ) {
    this.wasm = new WorkerRuntime(masterKey);
  }

  call<T>(operation: unknown, signal?: AbortSignal): Promise<T> {
    signal?.throwIfAborted();
    const id = this.wasm.submit(operation);
    const result = new Promise<T>((resolve, reject) => {
      this.operations.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
      });
    });
    const abort = () => {
      this.operations.delete(id);
      this.wasm.cancel(id);
      this.pump();
    };
    const abortResult = () => {
      this.operations.get(id)?.reject(signal!.reason);
      abort();
    };
    signal?.addEventListener("abort", abortResult, { once: true });
    this.pump();
    return result.finally(() =>
      signal?.removeEventListener("abort", abortResult),
    );
  }

  async requestExo(
    request: RawExoRequest,
    signal?: AbortSignal,
  ): Promise<RawExoResponse> {
    return this.call({ type: "request", request }, signal);
  }

  private pump(): void {
    const progress = this.wasm.poll() as Progress;
    for (const id of progress.cancelled) this.calls.get(id)?.abort();
    for (const completion of progress.completed) {
      const operation = this.operations.get(completion.id);
      if (!operation) continue;
      this.operations.delete(completion.id);
      if (completion.error !== null)
        operation.reject(new Error(completion.error));
      else operation.resolve(completion.result);
    }
    for (const event of progress.events) this.emitRuntimeEvent(event);
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
        let result: unknown;
        let message: string | undefined;
        try {
          result = await this.handle(call.request, controller.signal);
        } catch (error) {
          message = error instanceof Error ? error.message : String(error);
        }
        this.calls.delete(call.id);
        this.wasm.resolve(call.id, result, message);
        this.pump();
      };
      this.waitUntil(respond());
    }
  }
}
