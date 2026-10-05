import { DurableObject } from "cloudflare:workers";
import type {
  Agent,
  Conversation,
  Message,
  TurnRecord,
  VaultContext,
  Secret,
} from "../../typescript/harness/core";
import { DEFINITION_PATH, fields, text } from "./definition";
import type { Env, ExecRequest, SandboxIdentity, SandboxPolicy } from "./env";
import { PLACEHOLDER_PREFIX, proxyRequest, validateOrigin } from "./network";
import { CloudflareExoHarness, StateStore } from "./state";
import {
  threadRecord,
  artifactRecord,
  eventRecord,
  policyFromWire,
  secretRecord,
} from "./state-protocol";
import { Runtime } from "./runtime";
import { RuntimeIO, type HostRequest } from "./runtime-io";

export class ExoProvider extends DurableObject<Env> {
  readonly store: StateStore;
  readonly harness: CloudflareExoHarness;
  private readonly watchers = new Map<string, Set<() => void>>();
  private readonly runtime: Runtime;
  private readonly recovered: Promise<unknown>;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.store = new StateStore(ctx.storage.sql);
    this.harness = new CloudflareExoHarness(
      this.store,
      env.ARTIFACTS,
      env.VAULT_KEY,
      undefined,
      (threadId) => this.notify(threadId),
    );
    const io = new RuntimeIO(
      ctx,
      env,
      this.harness,
      (request, signal) => this.waitEvents(request, signal),
      (context, name, url) => this.credential(context, name, url),
    );
    this.runtime = new Runtime(
      (request, signal) => io.handle(request, signal),
      (promise) => ctx.waitUntil(promise),
    );
    this.recovered = this.runtime.call({ type: "recover" });
    ctx.waitUntil(this.recovered);
  }

  async sandboxPolicy(identity: SandboxIdentity): Promise<SandboxPolicy> {
    await this.conversation(identity);
    const policy = this.store.get<SandboxPolicy>(
      `policy/${identity.threadId}`,
    ) ?? {
      origins: [],
      credentials: [],
    };
    const model = this.store.get<SandboxPolicy>(
      `codex-policy/${identity.threadId}`,
    );
    return model
      ? {
          origins: [...new Set([...policy.origins, ...model.origins])],
          credentials: [
            ...policy.credentials.filter(
              (binding) => binding.environmentVariable !== "OPENAI_API_KEY",
            ),
            ...model.credentials,
          ],
        }
      : policy;
  }

  private async conversation(identity: SandboxIdentity): Promise<Conversation> {
    const agent = await this.harness.getAgent(identity.agentId);
    const thread = await agent?.getConversation(identity.threadId);
    if (!thread) throw new Error("thread not found");
    return thread;
  }

  private async credential(
    context: VaultContext,
    name: string,
    url: string,
  ): Promise<string> {
    const matches = [];
    for (const vault of await context.listVaults()) {
      for (const meta of await vault.listSecrets())
        if (meta.name === name || meta.id === name)
          matches.push({ vault, meta });
    }
    const match = matches.at(-1);
    if (!match)
      throw new Error("credential is missing in this thread's vaults");
    const resolved = await match.vault.resolveSecret(match.meta.id, {
      type: "url",
      url,
    });
    if (resolved.secret.type !== "key")
      throw new Error("only static key credentials are supported");
    return resolved.secret.value;
  }

  async proxy(identity: SandboxIdentity, request: Request): Promise<Response> {
    const thread = await this.conversation(identity);
    return proxyRequest(
      request,
      await this.sandboxPolicy(identity),
      (name, url) => this.credential(thread, name, url),
    );
  }

  private notify(threadId: string): void {
    for (const notify of this.watchers.get(threadId) ?? []) notify();
  }
  async alarm(): Promise<void> {
    await this.recovered;
    if (this.store.list<TurnRecord>("active/").length)
      await this.ctx.storage.setAlarm(Date.now() + 180_000);
  }

  private async waitEvents(
    request: Extract<HostRequest, { type: "watch" }>,
    signal: AbortSignal,
  ): Promise<unknown> {
    const thread = await this.conversation({
      agentId: request.agent_id,
      threadId: request.thread_id,
    });
    const watchers =
      this.watchers.get(request.thread_id) ?? new Set<() => void>();
    this.watchers.set(request.thread_id, watchers);
    // Register before reading so an append cannot fall between the read and wait.
    let wake!: () => void;
    const changed = new Promise<void>((resolve) => {
      wake = resolve;
    });
    watchers.add(wake);
    signal.addEventListener("abort", wake, { once: true });
    try {
      signal.throwIfAborted();
      let events = (
        await thread.getEvents({ cursor: request.after, limit: 1000 })
      ).events;
      if (!events.length) {
        await changed;
        signal.throwIfAborted();
        events = (
          await thread.getEvents({ cursor: request.after, limit: 1000 })
        ).events;
      }
      return { events: events.map(eventRecord) };
    } finally {
      watchers.delete(wake);
      signal.removeEventListener("abort", wake);
      if (!watchers.size) this.watchers.delete(request.thread_id);
    }
  }

  private watch(
    request: Request,
    thread: Conversation,
    after: string | null,
  ): Response {
    const encoder = new TextEncoder();
    let timer: ReturnType<typeof setInterval>;
    let notify: () => void;
    let canceled = false;
    let flushing = Promise.resolve();
    const watchers =
      this.watchers.get(thread.record.id) ?? new Set<() => void>();
    this.watchers.set(thread.record.id, watchers);
    const cleanup = () => {
      canceled = true;
      clearInterval(timer);
      watchers.delete(notify);
      if (!watchers.size) this.watchers.delete(thread.record.id);
    };
    const stream = new ReadableStream<Uint8Array>({
      start: (controller) => {
        notify = () => {
          flushing = flushing
            .then(async () => {
              if (canceled) return;
              const result = await thread.getEvents({ cursor: after });
              if (canceled) return;
              for (const event of result.events) {
                controller.enqueue(
                  encoder.encode(
                    `event: exo_event\ndata: ${JSON.stringify(eventRecord(event))}\n\n`,
                  ),
                );
                after = event.id;
              }
            })
            .catch(() => {
              if (!canceled) {
                cleanup();
                controller.close();
              }
            });
          this.ctx.waitUntil(flushing);
        };
        watchers.add(notify);
        notify();
        timer = setInterval(() => {
          if (!canceled) {
            controller.enqueue(encoder.encode(": heartbeat\n\n"));
            notify();
          }
        }, 10_000);
        request.signal.addEventListener(
          "abort",
          () => {
            if (!canceled) {
              cleanup();
              controller.close();
            }
          },
          { once: true },
        );
      },
      cancel: cleanup,
    });
    return new Response(stream, {
      headers: {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
      },
    });
  }

  async fetch(request: Request): Promise<Response> {
    const authenticated =
      this.env.ACCESS_AUD === undefined &&
      (await this.runtime.call<boolean>({
        type: "authenticate",
        token: this.env.EXO_TOKEN,
        authorization: request.headers.get("authorization"),
      }));
    if (!authenticated)
      return new Response("Unauthorized", {
        status: 401,
        headers: { "WWW-Authenticate": 'Bearer realm="exo"' },
      });
    return this.handleRequest(request);
  }

  // Trusted binding entry point: the front-door Worker has verified Access.
  async handleRequest(request: Request): Promise<Response> {
    try {
      return await this.route(request);
    } catch (error) {
      return Response.json(
        { error: error instanceof Error ? error.message : String(error) },
        { status: 400 },
      );
    }
  }

  private async route(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname
      .replace(/^\/exo\/?/, "")
      .split("/")
      .filter(Boolean)
      .map(decodeURIComponent);
    const method = request.method;
    const body = async <T>(names: string[]): Promise<T> =>
      fields(await request.json(), names) as T;
    if (method === "GET" && path[0] === "identity")
      return Response.json({ account_id: this.env.ACCOUNT_ID });
    if (method === "POST" && path[0] === "request") {
      const raw = await body<{ kind: "request"; id: number; request: unknown }>(
        ["kind", "id", "request"],
      );
      if (raw.kind !== "request" || !Number.isSafeInteger(raw.id))
        throw new Error("invalid state request envelope");
      try {
        const response = await this.runtime.call({
          type: "state",
          request: raw.request,
        });
        return Response.json({
          kind: "response",
          id: raw.id,
          ok: true,
          response,
          error: null,
        });
      } catch (error) {
        return Response.json({
          kind: "response",
          id: raw.id,
          ok: false,
          response: null,
          error: error instanceof Error ? error.message : String(error),
        });
      }
    }
    if (path[0] === "vault")
      return this.vaultRoute(request, this.harness, path.slice(1));
    if (path[0] !== "agent") return new Response("Not found", { status: 404 });
    if (path.length === 1) {
      if (method === "GET")
        return Response.json({
          agents: (await this.harness.listAgents())
            .map((agent) => agent.record)
            .filter(
              (agent) =>
                !url.searchParams.has("slug") ||
                agent.slug === url.searchParams.get("slug"),
            ),
        });
      if (method === "POST") {
        const raw = await body<{
          slug: string;
          name: string;
          vaults?: string[];
        }>(["slug", "name", "vaults"]);
        text(raw.name, "name");
        return Response.json((await this.harness.newAgent(raw)).record);
      }
    }
    const agent = await this.harness.getAgent(path[1]);
    if (!agent)
      return method === "GET" && path.length === 2
        ? Response.json(null)
        : new Response("Agent not found", { status: 404 });
    if (path.length === 2) {
      if (method === "GET") return Response.json(agent.record);
      if (method === "DELETE")
        return Response.json(await this.harness.deleteAgent(path[1]));
    }
    if (path[2] === "artifact")
      return this.artifactRoute(request, agent, path[3]);
    if (path[2] === "vault")
      return this.vaultRoute(request, agent, path.slice(3));
    if (path[2] !== "thread") return new Response("Not found", { status: 404 });
    if (path.length === 3) {
      if (method === "GET") {
        const limit = Number(url.searchParams.get("limit") ?? 100);
        if (!Number.isSafeInteger(limit) || limit < 1 || limit > 1000)
          throw new Error("invalid thread limit");
        const threads = (await agent.listConversations()).filter(
          (thread) =>
            !url.searchParams.has("cursor") ||
            (thread.record.latestEventId ?? thread.record.id) <
              url.searchParams.get("cursor")!,
        );
        return Response.json({
          agent: agent.record,
          threads: threads.slice(0, limit).map(threadRecord),
          next_cursor:
            threads.length > limit
              ? (threads[limit - 1].record.latestEventId ??
                threads[limit - 1].record.id)
              : null,
        });
      }
      if (method === "POST") {
        const raw = await body<{
          thread_slug?: string;
          thread_name?: string;
          vaults?: string[];
          harness?: string;
          model?: string;
          visibility?: string;
        }>([
          "thread_slug",
          "thread_name",
          "vaults",
          "harness",
          "model",
          "visibility",
        ]);
        if (raw.harness && !["basic", "codex"].includes(raw.harness))
          throw new Error("unsupported harness");
        if (raw.visibility && raw.visibility !== "owner")
          throw new Error("this prototype is a single operator deployment");
        await this.recovered;
        return Response.json(
          await this.runtime.call({
            type: "open_thread",
            agent_id: agent.record.id,
            request: {
              slug: raw.thread_slug,
              name: raw.thread_name,
              vaults: raw.vaults ?? [],
            },
            model: raw.model,
            harness: raw.harness,
          }),
        );
      }
    }
    const identity = { agentId: agent.record.id, threadId: path[3] };
    const thread = await agent.getConversation(identity.threadId);
    if (!thread) return new Response("Thread not found", { status: 404 });
    if (path.length === 4) {
      if (method === "GET")
        return Response.json({
          agent: agent.record,
          thread: threadRecord(thread),
        });
      if (method === "DELETE") {
        await this.env.SANDBOXES.getByName(identity.threadId).stop();
        const deleted = await agent.deleteConversation(identity.threadId);
        for (const key of ["policy", "codex-policy"])
          this.store.delete(`${key}/${identity.threadId}`);
        return Response.json({
          agent: agent.record,
          thread_id: identity.threadId,
          deleted,
        });
      }
    }
    if (path[4] === "vault")
      return this.vaultRoute(request, thread, path.slice(5));
    if (path[4] === "artifact")
      return this.artifactRoute(request, thread, path[5]);
    if (path[4] === "sandbox") {
      const sandbox = this.env.SANDBOXES.getByName(identity.threadId);
      if (path[5] === "policy") {
        if (method === "GET")
          return Response.json(await this.sandboxPolicy(identity));
        if (method === "PUT") {
          const raw = await body<{
            origins: string[];
            credentials: { environment_variable: string; credential: string }[];
          }>(["origins", "credentials"]);
          const origins = raw.origins.map(validateOrigin);
          const seen = new Set<string>();
          const credentials = raw.credentials.map((binding) => {
            fields(binding, ["environment_variable", "credential"]);
            if (
              !/^[A-Z_][A-Z0-9_]*$/.test(binding.environment_variable) ||
              seen.has(binding.environment_variable)
            )
              throw new Error(
                "invalid or duplicate credential environment variable",
              );
            seen.add(binding.environment_variable);
            return {
              environmentVariable: binding.environment_variable,
              credential: text(binding.credential, "credential"),
              placeholder: `${PLACEHOLDER_PREFIX}${crypto.randomUUID().replaceAll("-", "")}`,
            };
          });
          this.store.put(`policy/${identity.threadId}`, {
            origins,
            credentials,
          } satisfies SandboxPolicy);
          return Response.json({ updated: true });
        }
      }
      if (method === "POST" && path[5] === "exec")
        return Response.json(
          await sandbox.exec(
            identity,
            await body<ExecRequest>(["command", "env", "timeoutMs"]),
          ),
        );
      if (method === "POST" && path[5] === "snapshot")
        return Response.json(await sandbox.snapshot(identity));
      if (method === "POST" && path[5] === "stop") {
        await sandbox.stop();
        return Response.json({ stopped: true });
      }
    }
    if (path[4] === "session" && method === "POST")
      return Response.json(await thread.startSession());
    if (path[4] === "session" && method === "DELETE") {
      await thread.endSession(path[5]);
      return Response.json(true);
    }
    if (path[4] === "event" && method === "GET") {
      if (path[5] === "watch")
        return this.watch(request, thread, url.searchParams.get("after"));
      const direction = url.searchParams.get("direction") ?? "asc";
      if (direction !== "asc" && direction !== "desc")
        throw new Error("invalid event direction");
      const result = await thread.getEvents({
        cursor: url.searchParams.get("after"),
        limit: Number(url.searchParams.get("limit") ?? 100),
        direction,
        sessionId: url.searchParams.get("session_id"),
        turnId: url.searchParams.get("turn_id"),
        types: url.searchParams.has("event_type")
          ? [url.searchParams.get("event_type")!]
          : undefined,
      });
      return Response.json({
        events: result.events.map(eventRecord),
        cursor: result.cursor,
      });
    }
    if (path[4] === "turn") {
      if (method === "POST" && path.length === 5) {
        const raw = await body<{
          input?: Message | Message[];
          session_id?: string;
          model?: string;
          harness?: string;
          system_prompt?: string;
          attention?: string;
          reset_history?: boolean;
        }>([
          "input",
          "session_id",
          "model",
          "harness",
          "system_prompt",
          "attention",
          "reset_history",
        ]);
        if (
          (raw.harness && !["basic", "codex"].includes(raw.harness)) ||
          (raw.attention && raw.attention !== "wake") ||
          raw.reset_history
        )
          throw new Error("unsupported turn option");
        const input =
          raw.input === undefined
            ? []
            : Array.isArray(raw.input)
              ? raw.input
              : [raw.input];
        await this.recovered;
        await this.ctx.storage.setAlarm(Date.now() + 180_000);
        return Response.json(
          await this.runtime.call({
            type: "start_turn",
            agent_id: identity.agentId,
            thread_id: identity.threadId,
            request: { input, session_id: raw.session_id },
            model: raw.model,
            harness: raw.harness,
            system_prompt: raw.system_prompt,
          }),
          { status: 202 },
        );
      }
      const active = this.store.get<TurnRecord>(`active/${identity.threadId}`);
      if (method === "GET" && path.length === 6)
        return Response.json({ active: active?.id === path[5] });
      if (method === "POST" && path[6] === "cancel") {
        await this.recovered;
        return Response.json(
          await this.runtime.call({
            type: "cancel",
            thread_id: identity.threadId,
            turn_id: path[5],
          }),
        );
      }
      if (method === "POST" && path[6] === "approval-response") {
        const raw = await body<{
          session_id: string;
          approval_id: string;
          approved: boolean;
          allow_for_tool?: boolean;
        }>(["session_id", "approval_id", "approved", "allow_for_tool"]);
        await this.recovered;
        return Response.json(
          await this.runtime.call({
            type: "approve",
            agent_id: identity.agentId,
            thread_id: identity.threadId,
            turn_id: path[5],
            body: raw,
          }),
        );
      }
    }
    return new Response("Not found", { status: 404 });
  }

  private async artifactRoute(
    request: Request,
    target: Agent | Conversation,
    action?: string,
  ): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "GET" && !action)
      return Response.json((await target.listArtifacts()).map(artifactRecord));
    if (request.method === "GET" && action === "read") {
      const artifact = await target.readArtifact({
        artifactId: text(url.searchParams.get("artifact_id"), "artifact_id"),
        version: url.searchParams.has("version")
          ? Number(url.searchParams.get("version"))
          : undefined,
      });
      return Response.json(
        artifact
          ? {
              ...artifactRecord(artifact),
              contents: Array.from(artifact.contents),
            }
          : null,
      );
    }
    if (request.method === "POST" && !action) {
      const raw = fields(await request.json(), ["path", "contents"]);
      const path = text(raw.path, "path");
      if (
        !Array.isArray(raw.contents) ||
        raw.contents.some(
          (value) => !Number.isInteger(value) || value < 0 || value > 255,
        )
      )
        throw new Error("contents must be bytes");
      const contents = Uint8Array.from(raw.contents);
      if (path === DEFINITION_PATH && "newConversation" in target)
        return Response.json(
          await this.runtime.call({
            type: "configure_agent",
            agent_id: target.record.id,
            source: new TextDecoder().decode(contents),
          }),
        );
      return Response.json(
        artifactRecord(await target.writeArtifact({ path, contents })),
      );
    }
    return new Response("Not found", { status: 404 });
  }

  private async vaultRoute(
    request: Request,
    context: VaultContext,
    path: string[],
  ): Promise<Response> {
    if (!path.length) {
      if (request.method === "GET")
        return Response.json(
          (await context.listVaults()).map((vault) => ({
            id: vault.record.id,
            name: vault.record.name,
            created_at: vault.record.createdAt,
          })),
        );
      if (request.method === "POST" && context === this.harness) {
        const raw = fields(await request.json(), ["name"]);
        const vault = await this.harness.createVault(text(raw.name, "name"));
        return Response.json({
          id: vault.record.id,
          name: vault.record.name,
          created_at: vault.record.createdAt,
        });
      }
    }
    const vault = await context.getVault(path[0]);
    if (!vault)
      return new Response("Vault not found in scope", { status: 404 });
    if (
      path.length === 1 &&
      request.method === "DELETE" &&
      context === this.harness
    ) {
      await this.harness.deleteVault(path[0]);
      return Response.json(true);
    }
    if (path[1] === "secret") {
      if (request.method === "GET" && path.length === 2)
        return Response.json((await vault.listSecrets()).map(secretRecord));
      if (request.method === "POST" && path.length === 2) {
        const raw = fields(await request.json(), ["name", "secret", "policy"]);
        const secret = fields(raw.secret, ["type", "value"]);
        if (secret.type !== "key")
          throw new Error("only static key credentials are supported");
        return Response.json(
          await vault.putSecret({
            name: text(raw.name, "name"),
            secret: { type: "key", value: text(secret.value, "secret value") },
            policy:
              raw.policy === undefined ? undefined : policyFromWire(raw.policy),
          }),
        );
      }
      if (request.method === "PUT" && path.length === 3) {
        const raw = fields(await request.json(), ["secret", "policy"]);
        const secret =
          raw.secret === undefined
            ? undefined
            : fields(raw.secret, ["type", "value"]);
        if (
          secret &&
          (secret.type !== "key" || typeof secret.value !== "string")
        )
          throw new Error("only static key credentials are supported");
        return Response.json(
          secretRecord(
            await vault.updateSecret(path[2], {
              secret: secret as Secret | undefined,
              policy:
                raw.policy === undefined
                  ? undefined
                  : policyFromWire(raw.policy),
            }),
          ),
        );
      }
      if (request.method === "DELETE" && path.length === 3) {
        await vault.deleteSecret(path[2]);
        return Response.json(true);
      }
    }
    return new Response("Not found", { status: 404 });
  }
}
