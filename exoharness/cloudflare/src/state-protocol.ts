import type {
  Agent,
  Artifact,
  ArtifactVersion,
  Conversation,
  CredentialPolicy,
  CredentialDestination,
  Event,
  EventData,
  Secret,
  SecretMetadata,
  VaultContext,
} from "../../typescript/harness/core";
import { fields } from "./definition";
import { CloudflareExoHarness } from "./state";

export function threadRecord(thread: Conversation) {
  const { latestEventId, ...record } = thread.record;
  return { ...record, latest_event_id: latestEventId ?? null };
}
export function artifactRecord(meta: ArtifactVersion) {
  return {
    artifact_id: meta.artifactId,
    path: meta.path,
    version: meta.version,
    created_at: meta.createdAt,
    size_bytes: meta.sizeBytes,
  };
}
export function eventRecord(event: Event) {
  return {
    id: event.id,
    thread_id: event.conversationId,
    session_id: event.sessionId ?? null,
    turn_id: event.turnId ?? null,
    created_at: event.createdAt,
    data: event.data,
  };
}
export function policyFromWire(value: unknown): CredentialPolicy {
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
export function secretRecord(meta: SecretMetadata) {
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

export interface ResourceScope {
  type: "global" | "agent" | "thread";
  agent_id?: string;
  thread_id?: string;
}
interface StateArgs {
  slug?: string;
  name?: string;
  vaults?: string[];
  environment?: unknown;
  cursor?: string | null;
  limit?: number | null;
  unfinished_only?: boolean;
  artifact_id?: string;
  version?: number | null;
  path?: string;
  contents?: number[];
  session_id?: string | null;
  turn_id?: string | null;
  data?: EventData[];
  input?: import("../../typescript/harness/core").Message[];
  initial_events?: EventData[];
  up_to_inclusive?: string | null;
  secret?: Secret;
  policy?: unknown;
}
export interface StateRequest {
  type: string;
  agent_id?: string;
  conversation_id?: string;
  session_id?: string;
  turn_id?: string;
  event_id?: string;
  vault_id?: string;
  secret_id?: string;
  name?: string;
  scope?: ResourceScope;
  request?: StateArgs;
  query?: {
    cursor?: string | null;
    direction?: "asc" | "desc";
    limit?: number | null;
    session_id?: string | null;
    turn_id?: string | null;
    types?: string[] | null;
  } | null;
  data?: EventData[];
  vaults?: string[];
  secret?: Secret;
  policy?: unknown;
  target?: CredentialDestination;
}
function required<T>(value: T | null | undefined, name: string): T {
  if (value == null) throw new Error(`missing ${name}`);
  return value;
}
export async function stateRequest(
  harness: CloudflareExoHarness,
  ctx: DurableObjectState,
  q: StateRequest,
): Promise<unknown> {
  const r = q.request ?? {};
  const agent = async () =>
    required(await harness.getAgent(required(q.agent_id, "agent id")), "agent");
  const thread = async () =>
    required(
      await (
        await agent()
      ).getConversation(required(q.conversation_id, "thread id")),
      "thread",
    );
  const scope = async (): Promise<VaultContext> => {
    const scope = q.scope ?? { type: "global" };
    if (scope.type === "global") return harness;
    const a = required(
      await harness.getAgent(required(scope.agent_id, "scope agent id")),
      "agent",
    );
    if (scope.type === "agent") return a;
    return required(
      await a.getConversation(required(scope.thread_id, "scope thread id")),
      "thread",
    );
  };
  const vault = async () =>
    required(
      await (await scope()).getVault(required(q.vault_id, "vault id")),
      "vault",
    );
  const turn = async () =>
    (
      await harness.forTurn(
        required(q.agent_id, "agent id"),
        required(q.conversation_id, "thread id"),
        {
          id: required(q.turn_id, "turn id"),
          sessionId: required(q.session_id, "session id"),
        },
      )
    ).current.turn;
  const read = async (target: Agent | Conversation): Promise<Artifact | null> =>
    target.readArtifact({
      artifactId: required(r.artifact_id, "artifact id"),
      version: r.version ?? undefined,
    });
  const artifact = (value: Artifact | null) =>
    value && { ...artifactRecord(value), contents: Array.from(value.contents) };
  const write = (
    target: Agent | Conversation | import("../../typescript/harness/core").Turn,
  ) =>
    target.writeArtifact({
      path: required(r.path, "artifact path"),
      contents: Uint8Array.from(required(r.contents, "artifact contents")),
    });
  const conversation = (value: Conversation | null, agentId: string) =>
    value && { agent_id: agentId, record: threadRecord(value) };
  const added = (value: { eventIds: string[]; latestEventId: string }) => ({
    event_ids: value.eventIds,
    latest_event_id: value.latestEventId,
  });
  const vaultRecord = (
    value: { record: { id: string; name: string; createdAt: string } } | null,
  ) =>
    value && {
      id: value.record.id,
      name: value.record.name,
      created_at: value.record.createdAt,
    };
  switch (q.type) {
    case "list_environments":
      return { type: "environments", environments: [] };
    case "list_bindings":
      return { type: "bindings", bindings: [] };
    case "get_binding":
      return { type: "binding", binding: null };
    case "list_agents":
      return {
        type: "agents",
        agents: (await harness.listAgents()).map((a) => a.record),
      };
    case "get_agent":
      return {
        type: "agent",
        agent:
          (await harness.getAgent(required(q.agent_id, "agent id")))?.record ??
          null,
      };
    case "new_agent":
      return {
        type: "agent",
        agent: (
          await harness.newAgent({
            slug: required(r.slug, "slug"),
            name: required(r.name, "name"),
            vaults: r.vaults,
          })
        ).record,
      };
    case "delete_agent":
      return {
        type: "bool",
        value: await harness.deleteAgent(required(q.agent_id, "agent id")),
      };
    case "list_conversations": {
      let threads = await (await agent()).listConversations();
      if (r.unfinished_only)
        threads = threads.filter((t) =>
          harness.store.get(`active/${t.record.id}`),
        );
      if (r.cursor)
        threads = threads.filter(
          (t) => (t.record.latestEventId ?? t.record.id) < r.cursor!,
        );
      const limit = r.limit ?? 100;
      const selected = threads.slice(0, limit);
      return {
        type: "conversations",
        result: {
          conversations: selected.map((t) => conversation(t, q.agent_id!)),
          next_cursor:
            threads.length > limit
              ? (selected.at(-1)!.record.latestEventId ??
                selected.at(-1)!.record.id)
              : null,
        },
      };
    }
    case "get_conversation":
      return {
        type: "conversation",
        conversation: conversation(
          await (
            await agent()
          ).getConversation(required(q.conversation_id, "thread id")),
          q.agent_id!,
        ),
      };
    case "new_conversation": {
      if (r.environment)
        throw new Error(
          "environment definitions are not supported by this Worker",
        );
      return {
        type: "conversation",
        conversation: conversation(
          await (
            await agent()
          ).newConversation({ slug: r.slug, name: r.name, vaults: r.vaults }),
          q.agent_id!,
        ),
      };
    }
    case "delete_conversation":
      return {
        type: "bool",
        value: await (
          await agent()
        ).deleteConversation(required(q.conversation_id, "thread id")),
      };
    case "conversation_attach_vaults": {
      const t = await thread();
      harness.checkVaults(q.vaults ?? []);
      harness.store.put(`thread/${q.agent_id}/${q.conversation_id}`, {
        ...t.record,
        vaults: [...new Set([...t.record.vaults, ...(q.vaults ?? [])])],
      });
      return {
        type: "conversation",
        conversation: conversation(t, q.agent_id!),
      };
    }
    case "agent_list_artifacts":
      return {
        type: "artifact_versions",
        artifacts: (await (await agent()).listArtifacts()).map(artifactRecord),
      };
    case "agent_read_artifact":
      return {
        type: "artifact",
        artifact: artifact(await read(await agent())),
      };
    case "agent_write_artifact":
      return {
        type: "artifact_version",
        artifact: artifactRecord(await write(await agent())),
      };
    case "conversation_list_artifacts":
      return {
        type: "artifact_versions",
        artifacts: (await (await thread()).listArtifacts()).map(artifactRecord),
      };
    case "conversation_read_artifact":
      return {
        type: "artifact",
        artifact: artifact(await read(await thread())),
      };
    case "conversation_write_artifact":
      return {
        type: "artifact_version",
        artifact: artifactRecord(await write(await thread())),
      };
    case "conversation_start_session":
      return {
        type: "session_id",
        session_id: await (await thread()).startSession(),
      };
    case "conversation_end_session":
      await (await thread()).endSession(required(q.session_id, "session id"));
      return { type: "unit" };
    case "conversation_begin_turn": {
      const t = ctx.storage.transactionSync(() =>
        harness.beginTurnRecord(
          q.agent_id!,
          q.conversation_id!,
          r.input?.length ? [{ type: "messages", messages: r.input }] : [],
          r.session_id ?? undefined,
          r.initial_events ?? [],
        ),
      );
      return {
        type: "turn",
        turn: {
          conversation: conversation(await thread(), q.agent_id!),
          record: { id: t.record.id, session_id: t.record.sessionId },
        },
      };
    }
    case "conversation_get_events": {
      const query = q.query;
      const types = query?.types?.map((type) =>
        type === "conversation_created" ? "thread_created" : type,
      );
      const result = await (
        await thread()
      ).getEvents({
        cursor: query?.cursor,
        direction: query?.direction,
        limit: query?.limit,
        sessionId: query?.session_id,
        turnId: query?.turn_id,
        types,
      });
      return {
        type: "events",
        result: {
          events: result.events.map(eventRecord),
          cursor: result.cursor,
        },
      };
    }
    case "conversation_get_event":
      return {
        type: "event",
        event: ((e: Event | null) => e && eventRecord(e))(
          await (await thread()).getEvent(required(q.event_id, "event id")),
        ),
      };
    case "conversation_add_events":
      return {
        type: "add_events",
        result: added(
          await (
            await thread()
          ).addEvents({
            sessionId: r.session_id,
            turnId: r.turn_id,
            data: required(r.data, "events"),
          }),
        ),
      };
    case "conversation_fork":
      return {
        type: "conversation",
        conversation: conversation(
          await (
            await thread()
          ).fork({
            slug: r.slug,
            name: r.name,
            upToInclusive: r.up_to_inclusive,
          }),
          q.agent_id!,
        ),
      };
    case "turn_add_events":
      return {
        type: "add_events",
        result: added(
          await (await turn()).addEvents(required(q.data, "events")),
        ),
      };
    case "turn_write_artifact":
      return {
        type: "artifact_version",
        artifact: artifactRecord(await write(await turn())),
      };
    case "turn_finish":
      return { type: "event_id", event_id: await (await turn()).finish() };
    case "list_vaults":
      return {
        type: "vaults",
        vaults: (await (await scope()).listVaults()).map(vaultRecord),
      };
    case "get_vault":
      return {
        type: "vault",
        vault: vaultRecord(
          await (await scope()).getVault(required(q.vault_id, "vault id")),
        ),
      };
    case "create_vault":
      return {
        type: "vault",
        vault: vaultRecord(
          await harness.createVault(required(q.name, "vault name")),
        ),
      };
    case "delete_vault":
      await harness.deleteVault(required(q.vault_id, "vault id"));
      return { type: "bool", value: true };
    case "vault_list_secrets":
      return {
        type: "secrets",
        secrets: (await (await vault()).listSecrets()).map(secretRecord),
      };
    case "vault_get_secret":
      return {
        type: "secret",
        secret: await (
          await vault()
        ).getSecret(required(q.secret_id, "secret id")),
      };
    case "vault_put_secret":
      return {
        type: "secret_id",
        secret_id: await (
          await vault()
        ).putSecret({
          name: required(r.name, "secret name"),
          secret: required(r.secret, "secret"),
          policy: r.policy ? policyFromWire(r.policy) : undefined,
        }),
      };
    case "vault_update_secret":
      return {
        type: "secret_metadata",
        metadata: secretRecord(
          await (
            await vault()
          ).updateSecret(required(q.secret_id, "secret id"), {
            secret: q.secret,
            policy: q.policy ? policyFromWire(q.policy) : undefined,
          }),
        ),
      };
    case "vault_delete_secret":
      await (await vault()).deleteSecret(required(q.secret_id, "secret id"));
      return { type: "bool", value: true };
    case "vault_resolve_secret":
      return {
        type: "resolved_secret",
        ...(await (
          await vault()
        ).resolveSecret(
          required(q.secret_id, "secret id"),
          required(q.target, "destination"),
        )),
      };
    default:
      throw new Error(
        `state operation is not supported by this Worker: ${q.type}`,
      );
  }
}
