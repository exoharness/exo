import { DurableObject } from "cloudflare:workers";
import type {
  RawEvent,
  RawGetEventsResult,
} from "../../typescript/harness/client";
import type {
  Env,
  SandboxIdentity,
  SandboxPolicy,
  SandboxRequest,
} from "./env";
import { Runtime } from "./runtime";
import { RuntimeIO } from "./runtime-io";

export class ExoProvider extends DurableObject<Env> {
  private readonly runtime: Runtime;
  private readonly progress = new Map<string, Set<(event: RawEvent) => void>>();
  private readonly recovered: Promise<unknown>;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    let io: RuntimeIO;
    this.runtime = new Runtime(
      env.VAULT_KEY,
      (request, signal) => io.handle(request, signal),
      (promise) => ctx.waitUntil(promise),
      (event) => io.harnessProcesses.handleEvent(event),
    );
    io = new RuntimeIO(ctx, env, this.runtime, (threadId, event) => {
      for (const send of this.progress.get(threadId) ?? []) send(event);
    });
    this.recovered = this.runtime.call({ type: "recover" });
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

  async alarm(): Promise<void> {
    await this.recovered;
    if (await this.runtime.call<boolean>({ type: "has_unfinished_turns" }))
      await this.ctx.storage.setAlarm(Date.now() + 180_000);
  }

  async fetch(request: Request): Promise<Response> {
    if (
      this.env.ACCESS_AUD !== undefined ||
      !this.env.EXO_TOKEN ||
      request.headers.get("authorization") !== `Bearer ${this.env.EXO_TOKEN}`
    )
      return new Response("Unauthorized", {
        status: 401,
        headers: { "WWW-Authenticate": 'Bearer realm="exo"' },
      });
    return this.handleRequest(request);
  }

  // Trusted binding entry point after the front-door Worker verifies Access.
  async handleRequest(request: Request): Promise<Response> {
    try {
      await this.recovered;
      const url = new URL(request.url);
      const path = url.pathname
        .replace(/^\/exo\/?/, "")
        .split("/")
        .filter(Boolean)
        .map(decodeURIComponent);
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
      if (result.status === 202)
        await this.ctx.storage.setAlarm(Date.now() + 180_000);
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
        const emit = (event: RawEvent) => {
          if (!abort.signal.aborted)
            controller.enqueue(
              encoder.encode(
                `event: exo_event\ndata: ${JSON.stringify(event)}\n\n`,
              ),
            );
        };
        const listeners = this.progress.get(threadId) ?? new Set();
        this.progress.set(threadId, listeners);
        listeners.add(emit);
        const send = async () => {
          try {
            while (!abort.signal.aborted) {
              const page = await this.runtime.call<RawGetEventsResult>(
                {
                  type: "events",
                  agent_id: agentId,
                  thread_id: threadId,
                  after,
                },
                abort.signal,
              );
              for (const event of page.events as RawEvent[]) {
                abort.signal.throwIfAborted();
                emit(event);
                after = event.id;
              }
            }
          } catch (error) {
            if (!abort.signal.aborted) controller.error(error);
          } finally {
            listeners.delete(emit);
            if (!listeners.size) this.progress.delete(threadId);
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
