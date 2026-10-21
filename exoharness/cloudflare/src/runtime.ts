import { initSync, WorkerRuntime } from "./wasm/exo_worker_runtime";
import runtimeWasm from "./wasm/exo_worker_runtime_bg.wasm";
import type { HostRequest } from "./runtime-io";
import type { RawRuntimeEvent } from "../../typescript/harness/client";

initSync({ module: runtimeWasm });

export function threadObjectName(
  account: string,
  agent: string,
  thread: string,
): string {
  return WorkerRuntime.thread_name(account, agent, thread);
}

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
    {
      resolve(value: unknown): void;
      reject(error: Error): void;
    }
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
    private readonly discard: (
      request: HostRequest,
      result: unknown,
    ) => Promise<void>,
    thread = false,
  ) {
    this.wasm = new WorkerRuntime(masterKey, thread);
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

  stream(
    opening: Promise<number>,
    signal?: AbortSignal,
  ): ReadableStream<string> {
    const abort = new AbortController();
    let reader: ReadableStreamDefaultController<string> | undefined;
    const cancel = () => abort.abort(signal?.reason);
    signal?.addEventListener("abort", cancel, { once: true });
    let closing: Promise<unknown> | undefined;
    const close = () => {
      signal?.removeEventListener("abort", cancel);
      // Opening failures are reported to the reader and leave no handle to close.
      return (closing ??= opening.then(
        (id) => this.call({ type: "watch_close", id }),
        () => {},
      ));
    };
    abort.signal.addEventListener(
      "abort",
      () => {
        reader?.error(abort.signal.reason);
        this.waitUntil(close());
      },
      {
        once: true,
      },
    );
    if (signal?.aborted) cancel();
    return new ReadableStream<string>({
      start: (controller) => {
        reader = controller;
        if (abort.signal.aborted) controller.error(abort.signal.reason);
      },
      pull: async (controller) => {
        try {
          const id = await opening;
          abort.signal.throwIfAborted();
          const frame = await this.call<string | null>(
            { type: "watch_next", id },
            abort.signal,
          );
          if (frame === null) {
            controller.close();
            await close();
          } else controller.enqueue(frame);
        } catch (error) {
          controller.error(error);
          cancel();
        }
      },
      cancel,
    });
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
      // A pending watch read keeps the SSE connection's object resident.
      // Keeping the object resident preserves its live progress subscription;
      // disconnecting cancels the watch and releases this waitUntil hold.
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
        if (!this.wasm.resolve(call.id, result, message)) {
          try {
            await this.discard(call.request, result);
          } catch (error) {
            console.error("Abandoned host call cleanup failed", error);
          }
        }
        this.pump();
      };
      this.waitUntil(respond());
    }
  }
}
