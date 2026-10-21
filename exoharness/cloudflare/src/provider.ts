import { DurableObject, RpcTarget } from "cloudflare:workers";
import type { Env, SandboxIdentity } from "./env";
import { Runtime } from "./runtime";
import { RuntimeIO } from "./runtime-io";

class RuntimeObject extends DurableObject<Env> {
  protected readonly runtime: Runtime;
  protected recovered?: Promise<boolean>;

  constructor(ctx: DurableObjectState, env: Env, thread = false) {
    super(ctx, env);
    let io: RuntimeIO;
    this.runtime = new Runtime(
      env.VAULT_KEY,
      (request, signal) => io.handle(request, signal),
      (promise) => ctx.waitUntil(promise),
      (event) => io.harnessProcesses.handleEvent(event),
      (request, result) => io.discard(request, result),
      thread,
    );
    io = new RuntimeIO(ctx, env, this.runtime);
    if (!thread) this.recovered = Promise.resolve(false);
    else
      ctx.waitUntil(
        this.recover().catch((error) =>
          console.error("Turn recovery failed", error),
        ),
      );
  }

  protected recover(): Promise<boolean> {
    return (this.recovered ??= this.runtime
      .call<boolean>({ type: "drain_queue" })
      .catch((error) => {
        this.recovered = undefined;
        throw error;
      }));
  }

  async proxyHeaders(
    identity: SandboxIdentity,
    request: import("./network").ProxyRequest,
    rejected: [string, string][] | null = null,
  ): Promise<import("./network").ProxyReply> {
    return this.runtime.call({
      type: "proxy_headers",
      agent_id: identity.agentId,
      thread_id: identity.threadId,
      sandbox_id: identity.sandboxId,
      url: request.url,
      method: request.method,
      headers: request.headers,
      rejected,
    });
  }

  // Trusted binding entry point after the front-door Worker verifies Access.
  async handleRequest(request: Request): Promise<Response> {
    let watchAbort: AbortController | undefined;
    try {
      await this.recover();
      const url = new URL(request.url);
      const result = await this.runtime.call<
        { status: number; body: string } | { watch: number }
      >(
        {
          type: "http",
          account_id: this.env.ACCOUNT_ID,
          request: {
            method: request.method,
            path: requestPath(url),
            query: url.search.slice(1),
            body: await request.text(),
          },
        },
        request.signal,
      );
      if ("watch" in result) {
        watchAbort = new AbortController();
        return new Response(
          this.runtime
            .stream(
              Promise.resolve(result.watch),
              AbortSignal.any([request.signal, watchAbort.signal]),
            )
            .pipeThrough(new TextEncoderStream()),
          {
            headers: {
              "content-type": "text/event-stream; charset=utf-8",
              "cache-control": "no-cache, no-transform",
            },
          },
        );
      }
      return new Response(result.body, {
        status: result.status,
        headers: { "content-type": "application/json" },
      });
    } catch (error) {
      // Also cancel a watch opened before constructing its HTTP response.
      watchAbort?.abort(error);
      return Response.json(
        { error: error instanceof Error ? error.message : String(error) },
        { status: 500 },
      );
    }
  }
}

export function requestPath(url: URL): string[] {
  return url.pathname
    .replace(/^\/exo\/?/, "")
    .split("/")
    .filter(Boolean)
    .map(decodeURIComponent);
}

/** Shared records, vaults and the existing Rust sandbox manager. */
export class ExoProvider extends RuntimeObject {
  // Trusted internal protocol transport; all store semantics remain in Rust.
  requestExo(request: unknown): Promise<unknown> {
    return this.runtime.call({ type: "request", request });
  }

  subscribe(
    agent_id: string,
    thread_id: string,
    after: unknown,
  ): EventSubscription {
    return new EventSubscription(this.runtime, agent_id, thread_id, after);
  }
}

/** Single owner of one thread's durable coordinator and Rust runtime. */
export class ExoThread extends RuntimeObject {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env, true);
  }
  async alarm(): Promise<void> {
    this.recovered = undefined;
    if (await this.recover())
      await this.ctx.storage.setAlarm(Date.now() + 180_000);
  }
}

/** Disposable RPC subscription, so aborting a thread watch closes its account watch. */
class EventSubscription extends RpcTarget {
  private readonly abort = new AbortController();
  private readonly reader: ReadableStreamDefaultReader<string>;
  constructor(
    runtime: Runtime,
    agent_id: string,
    thread_id: string,
    after: unknown,
  ) {
    super();
    const stream = runtime.stream(
      runtime.call<number>(
        { type: "watch_state", agent_id, thread_id, after },
        this.abort.signal,
      ),
      this.abort.signal,
    );
    this.reader = stream.getReader();
  }
  async next(): Promise<string | null> {
    const { done, value } = await this.reader.read();
    return done ? null : value;
  }
  [Symbol.dispose](): void {
    this.abort.abort();
  }
}
