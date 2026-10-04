import { DurableObject } from "cloudflare:workers";
import { init as initLingua } from "@braintrust/lingua/browser";
import linguaWasm from "@braintrust/lingua-wasm/browser/lingua_bg.wasm";
import {
  materializePromptMessages,
  messagesEvent,
  systemTextMessage,
  toolResultEvent,
  type Agent,
  type ArtifactVersion,
  type Conversation,
  type Event,
  type Message,
  type CredentialPolicy,
  type CredentialDestination,
  type Secret,
  type SecretMetadata,
  type TurnRecord,
  type VaultContext,
  type PendingToolCall,
} from "../../typescript/harness/core";
import {
  ResponsesRuntime,
  responseToLinguaEvents,
  responseToolCalls,
} from "../../typescript/model-runtime/responses";
import {
  DEFINITION_PATH,
  fields,
  parseDefinition,
  text,
  type Definition,
} from "./definition";
import type { Env, ExecRequest, SandboxIdentity, SandboxPolicy } from "./env";
import { PLACEHOLDER_PREFIX, proxyRequest, validateOrigin } from "./network";
import { CloudflareExoHarness, StateStore } from "./state";

interface Job extends SandboxIdentity {
  turn: TurnRecord;
  definition: Definition;
  round: number;
  phase: "model" | "tools" | "executing" | "approval";
  tools: PendingToolCall[];
  approvalId?: string;
}

const shell = {
  name: "shell",
  description:
    "Run a shell command in the thread's Linux sandbox. Working directory: /workspace. Internet access is restricted by the thread's policy.",
  parameters: {
    type: "object",
    properties: { command: { type: "string" } },
    required: ["command"],
    additionalProperties: false,
  },
  strict: true,
};

function threadRecord(thread: Conversation) {
  const { latestEventId, ...record } = thread.record;
  return { ...record, latest_event_id: latestEventId ?? null };
}
function artifactRecord(meta: ArtifactVersion) {
  return {
    artifact_id: meta.artifactId,
    path: meta.path,
    version: meta.version,
    created_at: meta.createdAt,
    size_bytes: meta.sizeBytes,
  };
}
function eventRecord(event: Event) {
  return {
    id: event.id,
    thread_id: event.conversationId,
    session_id: event.sessionId ?? null,
    turn_id: event.turnId ?? null,
    created_at: event.createdAt,
    data: event.data,
  };
}
function policyFromWire(value: unknown): CredentialPolicy {
  const raw = fields(value, ["networking", "injection_location"]);
  const network = fields(raw.networking, [
    "type",
    "allowed_hosts",
    "allowed_destinations",
  ]);
  const injection = fields(raw.injection_location, ["header"]);
  return {
    networking:
      network.type === "limited"
        ? { type: "limited", allowedHosts: network.allowed_hosts as string[] }
        : network.type === "destinations"
          ? {
              type: "destinations",
              allowedDestinations:
                network.allowed_destinations as CredentialDestination[],
            }
          : (() => {
              throw new Error("invalid credential policy");
            })(),
    injectionLocation: { header: injection.header as boolean },
  };
}
function secretRecord(meta: SecretMetadata) {
  const policy = meta.policy;
  return {
    id: meta.id,
    name: meta.name,
    type: meta.type,
    revision: meta.revision,
    created_at: meta.createdAt,
    policy: policy
      ? {
          networking:
            policy.networking.type === "limited"
              ? {
                  type: "limited",
                  allowed_hosts: policy.networking.allowedHosts,
                }
              : {
                  type: "destinations",
                  allowed_destinations: policy.networking.allowedDestinations,
                },
          injection_location: policy.injectionLocation,
        }
      : null,
  };
}

export class ExoProvider extends DurableObject<Env> {
  readonly store: StateStore;
  readonly harness: CloudflareExoHarness;
  private readonly running = new Set<string>();
  private readonly watchers = new Map<string, Set<() => void>>();

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.store = new StateStore(ctx.storage.sql);
    this.harness = new CloudflareExoHarness(
      this.store,
      env.ARTIFACTS,
      env.VAULT_KEY,
    );
  }

  async sandboxPolicy(identity: SandboxIdentity): Promise<SandboxPolicy> {
    await this.conversation(identity);
    return (
      this.store.get<SandboxPolicy>(`policy/${identity.threadId}`) ?? {
        origins: [],
        credentials: [],
      }
    );
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
    const parts = name.split("/");
    if (parts.length > 2) throw new Error("credential must be [vault/]name");
    const matches = [];
    for (const vault of await context.listVaults()) {
      if (parts.length === 2 && vault.record.name !== parts[0]) continue;
      for (const meta of await vault.listSecrets())
        if (meta.name === parts.at(-1)) matches.push({ vault, meta });
    }
    if (matches.length !== 1)
      throw new Error(
        "credential is missing or ambiguous in this thread's vaults",
      );
    const resolved = await matches[0].vault.resolveSecret(matches[0].meta.id, {
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
  private async wake(job: Job): Promise<void> {
    // Alarm survives object eviction and recovers an interrupted model round.
    await this.ctx.storage.setAlarm(Date.now() + 180_000);
    this.ctx.waitUntil(this.drive(job.threadId));
  }

  async alarm(): Promise<void> {
    const jobs = this.store
      .list<Job>("job/")
      .filter((job) => job.phase !== "approval");
    for (const job of jobs) await this.drive(job.threadId);
    if (this.store.list<Job>("job/").some((job) => job.phase !== "approval"))
      await this.ctx.storage.setAlarm(Date.now() + 180_000);
  }

  private async drive(threadId: string): Promise<void> {
    if (this.running.has(threadId)) return;
    this.running.add(threadId);
    let job = this.store.get<Job>(`job/${threadId}`);
    try {
      while (job && job.phase !== "approval") {
        const harness = await this.harness.forTurn(
          job.agentId,
          threadId,
          job.turn,
        );
        const { conversation, turn } = harness.current;
        if (
          this.store.get<TurnRecord>(`active/${threadId}`)?.id !== job.turn.id
        )
          return;
        if (job.phase === "executing")
          throw new Error(
            "sandbox execution was interrupted; its outcome is unknown and the command will not be replayed",
          );
        if (job.phase === "model") {
          if (job.round > (job.definition.config.max_tool_round_trips ?? 20))
            throw new Error("tool round budget exceeded");
          const baseURL =
            job.definition.config.base_url ?? "https://api.openai.com/v1";
          const endpoint = `${baseURL.replace(/\/$/, "")}/responses`;
          const apiKey = await this.credential(
            conversation,
            job.definition.config.credential ?? "OPENAI_API_KEY",
            endpoint,
          );
          const runtime = new ResponsesRuntime({ apiKey, baseURL });
          await initLingua(linguaWasm);
          const response = await runtime.complete({
            model: job.definition.config.model,
            messages: await materializePromptMessages(conversation, [
              systemTextMessage(
                `You are ${job.definition.name}.\n\n${job.definition.instructions}`,
              ),
            ]),
            tools: [shell],
            maxOutputTokens: job.definition.config.max_output_tokens,
          });
          if (
            this.store.get<TurnRecord>(`active/${threadId}`)?.id !== job.turn.id
          )
            return;
          job.tools = responseToolCalls(response);
          const roundJob = job;
          this.ctx.storage.transactionSync(() => {
            turn.appendEvents(responseToLinguaEvents(response));
            if (!roundJob.tools.length) {
              turn.finishRecord();
              this.store.delete(`job/${threadId}`);
            } else {
              roundJob.phase = "tools";
              roundJob.round += 1;
              this.store.put(`job/${threadId}`, roundJob);
            }
          });
          if (!job.tools.length) {
            this.notify(threadId);
            return;
          }
          this.notify(threadId);
        } else {
          const tool = job.tools[0];
          if (!tool) {
            job.phase = "model";
            this.store.put(`job/${threadId}`, job);
            continue;
          }
          if (tool.request.functionName !== "shell")
            throw new Error(`unsupported tool: ${tool.request.functionName}`);
          const args = fields(tool.request.arguments, ["command"]);
          const command = text(args.command, "shell command");
          if (
            job.definition.askShell &&
            !job.approvalId &&
            !this.store.get(`allow-shell/${threadId}/${job.turn.sessionId}`)
          ) {
            job.approvalId = this.store.id();
            job.phase = "approval";
            this.store.put(`job/${threadId}`, job);
            await turn.addEvents([
              {
                type: "custom",
                event_type: "agent_runtime.approval_requested",
                payload: {
                  approval_id: job.approvalId,
                  tool_call_id: tool.toolCallId,
                  round: job.round,
                  request: {
                    function_name: "shell",
                    arguments: tool.request.arguments,
                  },
                },
              },
            ]);
            this.notify(threadId);
            return;
          }
          job.phase = "executing";
          this.store.put(`job/${threadId}`, job);
          const output = await this.env.SANDBOXES.getByName(threadId).exec(
            { agentId: job.agentId, threadId },
            { command: ["sh", "-lc", command] },
          );
          if (
            this.store.get<TurnRecord>(`active/${threadId}`)?.id !== job.turn.id
          )
            return;
          const toolJob = job;
          this.ctx.storage.transactionSync(() => {
            turn.appendEvents([
              toolResultEvent(tool.toolCallId, { ...output }),
            ]);
            toolJob.tools.shift();
            toolJob.approvalId = undefined;
            toolJob.phase = "tools";
            this.store.put(`job/${threadId}`, toolJob);
          });
          this.notify(threadId);
        }
        job = this.store.get<Job>(`job/${threadId}`);
      }
    } catch (error) {
      if (
        job &&
        this.store.get<TurnRecord>(`active/${threadId}`)?.id === job.turn.id
      ) {
        if (job.phase === "executing")
          await this.env.SANDBOXES.getByName(threadId).stop();
        const harness = await this.harness.forTurn(
          job.agentId,
          threadId,
          job.turn,
        );
        await harness.current.turn.addEvents([
          {
            type: "error",
            message: error instanceof Error ? error.message : String(error),
          },
        ]);
        await harness.current.turn.finish();
        this.store.delete(`job/${threadId}`);
        this.notify(threadId);
      }
    } finally {
      this.running.delete(threadId);
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
            thread.record.id > url.searchParams.get("cursor")!,
        );
        return Response.json({
          agent: agent.record,
          threads: threads.slice(0, limit).map(threadRecord),
          next_cursor:
            threads.length > limit ? threads[limit - 1].record.id : null,
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
        if (raw.harness && raw.harness !== "basic")
          throw new Error("only the basic harness is supported");
        if (raw.visibility && raw.visibility !== "owner")
          throw new Error("this prototype is a single operator deployment");
        await this.definition(agent);
        const thread = await agent.newConversation({
          slug: raw.thread_slug,
          name: raw.thread_name,
          vaults: raw.vaults,
        });
        if (raw.model)
          this.store.put(`model/${thread.record.id}`, text(raw.model, "model"));
        return Response.json({
          agent: agent.record,
          thread: threadRecord(thread),
          harness: "basic",
        });
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
        for (const key of ["model", "policy", "job"])
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
          (raw.harness && raw.harness !== "basic") ||
          (raw.attention && raw.attention !== "wake") ||
          raw.reset_history
        )
          throw new Error("unsupported turn option");
        const definition = await this.definition(agent);
        definition.config.model =
          raw.model ??
          this.store.get<string>(`model/${identity.threadId}`) ??
          definition.config.model;
        if (raw.system_prompt !== undefined)
          definition.instructions = text(raw.system_prompt, "system_prompt");
        const input =
          raw.input === undefined
            ? []
            : Array.isArray(raw.input)
              ? raw.input
              : [raw.input];
        if (input.some((message) => message.role !== "user"))
          throw new Error("turn input must contain user messages");
        const job: Job = this.ctx.storage.transactionSync(() => {
          const turn = this.harness.beginTurnRecord(
            identity.agentId,
            identity.threadId,
            input.length ? [messagesEvent(input)] : [],
            raw.session_id,
          );
          const job: Job = {
            ...identity,
            turn: turn.record,
            definition,
            round: 0,
            phase: "model",
            tools: [],
          };
          this.store.put(`job/${identity.threadId}`, job);
          return job;
        });
        await this.wake(job);
        this.notify(identity.threadId);
        return Response.json(
          {
            agent: agent.record,
            thread: threadRecord(thread),
            turn: { id: job.turn.id, session_id: job.turn.sessionId },
            harness: "basic",
          },
          { status: 202 },
        );
      }
      const active = this.store.get<TurnRecord>(`active/${identity.threadId}`);
      if (method === "GET" && path.length === 6)
        return Response.json({ active: active?.id === path[5] });
      if (method === "POST" && path[6] === "cancel") {
        let finished: string | null = null;
        if (active?.id === path[5]) {
          this.store.delete(`job/${identity.threadId}`);
          const context = await this.harness.forTurn(
            identity.agentId,
            identity.threadId,
            active,
          );
          finished = await context.current.turn.finish();
          await this.env.SANDBOXES.getByName(identity.threadId).stop();
          this.notify(identity.threadId);
        }
        return Response.json({
          canceled_active_turn: finished !== null,
          finished_event_id: finished,
        });
      }
      if (method === "POST" && path[6] === "approval-response") {
        const raw = await body<{
          session_id: string;
          approval_id: string;
          approved: boolean;
          allow_for_tool?: boolean;
        }>(["session_id", "approval_id", "approved", "allow_for_tool"]);
        const job = this.store.get<Job>(`job/${identity.threadId}`);
        if (
          !job ||
          active?.id !== path[5] ||
          job.turn.sessionId !== raw.session_id ||
          job.phase !== "approval" ||
          job.approvalId !== raw.approval_id
        )
          throw new Error("approval is not pending for this session and turn");
        if (
          typeof raw.approved !== "boolean" ||
          (raw.allow_for_tool && !raw.approved)
        )
          throw new Error("invalid approval response");
        const context = await this.harness.forTurn(
          identity.agentId,
          identity.threadId,
          job.turn,
        );
        const result = await context.current.turn.addEvents([
          {
            type: "custom",
            event_type: "agent_runtime.approval_response",
            payload: {
              approval_id: raw.approval_id,
              approved: raw.approved,
              allowed_tool_name: raw.allow_for_tool ? "shell" : null,
            },
          },
        ]);
        if (raw.allow_for_tool)
          this.store.put(
            `allow-shell/${identity.threadId}/${raw.session_id}`,
            true,
          );
        if (!raw.approved) {
          await context.current.turn.addEvents([
            toolResultEvent(job.tools[0].toolCallId, {
              ok: false,
              error: "tool execution denied by user",
            }),
          ]);
          job.tools.shift();
          job.approvalId = undefined;
        }
        job.phase = "tools";
        this.store.put(`job/${identity.threadId}`, job);
        await this.wake(job);
        this.notify(identity.threadId);
        return Response.json({ event_id: result.latestEventId });
      }
    }
    return new Response("Not found", { status: 404 });
  }

  private async definition(agent: Agent): Promise<Definition> {
    const meta = (await agent.listArtifacts()).find(
      (meta) => meta.path === DEFINITION_PATH,
    );
    if (!meta) throw new Error("save a managed agent definition first");
    return parseDefinition(
      (await agent.readArtifactText({ artifactId: meta.artifactId }))!,
    );
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
      if (path === DEFINITION_PATH)
        parseDefinition(new TextDecoder().decode(contents));
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
