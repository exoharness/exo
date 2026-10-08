import { DurableObject, RpcTarget } from "cloudflare:workers";
import type {
  RawExoRequest,
  RawExoResponse,
  RawEvent,
} from "../../typescript/harness/client";
import type {
  Env,
  SandboxIdentity,
  SandboxPolicy,
  SandboxRequest,
} from "./env";
import { Runtime } from "./runtime";
import { RuntimeIO } from "./runtime-io";

class RuntimeObject extends DurableObject<Env> {
  protected readonly runtime: Runtime;
  private readonly recovered: Promise<unknown>;

  constructor(ctx: DurableObjectState, env: Env, thread = false) {
    super(ctx, env);
    let io: RuntimeIO;
    this.runtime = new Runtime(
      env.VAULT_KEY,
      (request, signal) => io.handle(request, signal),
      (promise) => ctx.waitUntil(promise),
      (event) => io.harnessProcesses.handleEvent(event),
      thread,
    );
    io = new RuntimeIO(ctx, env, this.runtime);
    this.recovered = thread
      ? this.runtime.call({ type: "recover" })
      : Promise.resolve();
    ctx.waitUntil(this.recovered);
  }

  async sandboxPolicy(request: SandboxRequest): Promise<SandboxPolicy> {
    return this.runtime.call({
      type: "sandbox_policy",
      request,
    });
  }

  async proxy(identity: SandboxIdentity, request: Request): Promise<Response> {
    const headers = await this.runtime.call<[string, string][]>({
      type: "proxy_headers",
      agent_id: identity.agentId,
      thread_id: identity.threadId,
      sandbox_id: identity.sandboxId,
      url: request.url,
      method: request.method,
      headers: [...request.headers],
    });
    // Redirects return to the sandbox so every destination is checked again.
    return fetch(
      new Request(request.url, {
        method: request.method,
        headers,
        body: request.body,
        redirect: "manual",
      }),
    );
  }

  async fetch(request: Request): Promise<Response> {
    const denied = staticAuthorization(this.env, request);
    if (denied) return denied;
    return this.handleRequest(request);
  }

  // Trusted binding entry point after the front-door Worker verifies Access.
  async handleRequest(request: Request): Promise<Response> {
    try {
      await this.recovered;
      const url = new URL(request.url);
      const path = requestPath(request);
      if (
        request.method === "GET" &&
        path.length === 1 &&
        path[0] === "identity"
      )
        return Response.json({ account_id: this.env.ACCOUNT_ID });
      if (
        request.method === "GET" &&
        path.length === 6 &&
        path[0] === "agent" &&
        path[2] === "thread" &&
        path[4] === "event" &&
        path[5] === "watch"
      ) {
        const thread = await this.http("GET", path.slice(0, 4), "", "");
        if (thread.status !== 200)
          return new Response(thread.body, {
            status: thread.status,
            headers: { "content-type": "application/json" },
          });
        return this.watch(
          request,
          path[1],
          path[3],
          url.searchParams.get("after"),
        );
      }
      const result = await this.http(
        request.method,
        path,
        url.search.slice(1),
        await request.text(),
      );
      return new Response(result.body, {
        status: result.status,
        headers: { "content-type": "application/json" },
      });
    } catch (error) {
      return Response.json(
        { error: error instanceof Error ? error.message : String(error) },
        { status: 400 },
      );
    }
  }

  private http(
    method: string,
    path: string[],
    query: string,
    body: string,
  ): Promise<{ status: number; body: string }> {
    return this.runtime.call({
      type: "http",
      request: { method, path, query, body },
    });
  }

  private watch(
    request: Request,
    agentId: string,
    threadId: string,
    after: string | null,
  ): Response {
    const encoder = new TextEncoder();
    const abort = new AbortController();
    const cancel = () => abort.abort();
    request.signal.addEventListener("abort", cancel, { once: true });
    if (request.signal.aborted) cancel();
    let timer: ReturnType<typeof setInterval>;
    const stream = new ReadableStream<Uint8Array>({
      start: (controller) => {
        timer = setInterval(() => {
          if (!abort.signal.aborted)
            controller.enqueue(encoder.encode(": heartbeat\n\n"));
        }, 10_000);
        const send = async () => {
          try {
            await this.runtime.call(
              { type: "watch", agent_id: agentId, thread_id: threadId, after },
              abort.signal,
              (event) =>
                controller.enqueue(
                  encoder.encode(
                    `event: exo_event\ndata: ${JSON.stringify(event)}\n\n`,
                  ),
                ),
            );
            if (!abort.signal.aborted) controller.close();
          } catch (error) {
            if (!abort.signal.aborted) controller.error(error);
          } finally {
            clearInterval(timer);
            request.signal.removeEventListener("abort", cancel);
          }
        };
        this.ctx.waitUntil(send());
      },
      cancel,
    });
    return new Response(stream, {
      headers: {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
      },
    });
  }
}

export function staticAuthorization(
  env: Env,
  request: Request,
): Response | undefined {
  if (
    env.ACCESS_AUD !== undefined ||
    !env.EXO_TOKEN ||
    request.headers.get("authorization") !== `Bearer ${env.EXO_TOKEN}`
  )
    return new Response("Unauthorized", {
      status: 401,
      headers: { "WWW-Authenticate": 'Bearer realm="exo"' },
    });
}

export function requestPath(request: Request): string[] {
  return new URL(request.url).pathname
    .replace(/^\/exo\/?/, "")
    .split("/")
    .filter(Boolean)
    .map(decodeURIComponent);
}

/** Shared records, vaults and the existing Rust sandbox manager. */
export class ExoProvider extends RuntimeObject {
  // Trusted internal protocol transport; all store semantics remain in Rust.
  requestExo(request: RawExoRequest): Promise<RawExoResponse> {
    return this.runtime.requestExo(request);
  }

  subscribe(
    agent_id: string,
    thread_id: string,
    after: unknown,
  ): EventSubscription {
    return new EventSubscription(
      this.runtime,
      this.ctx,
      agent_id,
      thread_id,
      after,
    );
  }
}

/** Single owner of one thread's durable coordinator and Rust runtime. */
export class ExoThread extends RuntimeObject {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env, true);
  }
  async alarm(): Promise<void> {
    await this.runtime.call({ type: "recover" });
    if (await this.runtime.call<boolean>({ type: "has_pending_turns" }))
      await this.ctx.storage.setAlarm(Date.now() + 180_000);
  }
}

/** Disposable RPC subscription, so aborting a thread watch closes its account watch. */
class EventSubscription extends RpcTarget {
  private readonly abort = new AbortController();
  private readonly reader: ReadableStreamDefaultReader<RawEvent>;
  constructor(
    runtime: Runtime,
    private readonly ctx: DurableObjectState,
    agent_id: string,
    thread_id: string,
    after: unknown,
  ) {
    super();
    const stream = new ReadableStream<RawEvent>({
      start: (controller) => {
        ctx.waitUntil(
          runtime
            .call(
              { type: "watch_state", agent_id, thread_id, after },
              this.abort.signal,
              (event) => controller.enqueue(event),
            )
            .then(
              () => {
                if (!this.abort.signal.aborted) controller.close();
              },
              (error) => {
                if (!this.abort.signal.aborted) controller.error(error);
              },
            ),
        );
      },
    });
    this.reader = stream.getReader();
  }
  async next(): Promise<RawEvent | null> {
    const { done, value } = await this.reader.read();
    return done ? null : value;
  }
  async close(): Promise<void> {
    this.abort.abort();
    await this.reader.cancel();
  }
  [Symbol.dispose](): void {
    this.abort.abort();
    this.ctx.waitUntil(this.reader.cancel());
  }
}
