import {
  toAgentConfig,
  toConversationConfig,
  toSendRequest,
  type RawAgentConfig,
  type RawConversationConfig,
  type RawSendRequest,
} from "./wire";

import {
  asBytes,
  toolResultEvent,
  type AddEventsRequest,
  type AddEventsResult,
  type Agent,
  type AgentRecord,
  type Artifact,
  type ArtifactVersion,
  type Conversation,
  type ConversationRecord,
  type Event,
  type EventData,
  type EventQuery,
  type ExoHarnessCurrent,
  type ExoHarness,
  type ForkConversationRequest,
  type GetEventsResult,
  type JsonObject,
  type NewConversationRequest,
  type PendingToolCall,
  type SandboxProcess,
  type SandboxProcessStartRequest,
  type Secret,
  type SecretMetadata,
  type CredentialDestination,
  type CredentialPolicy,
  type Vault,
  type VaultContext,
  type ToolDefinition,
  type ToolRequest,
  type ToolResult,
  type Turn,
  type TurnContext,
  type NativeMcpServer,
  type TurnRecord,
} from "./index";

export interface RawToolRequest {
  function_name: string;
  arguments: JsonObject;
}

export interface RawAgentRecord {
  vaults?: string[];
  id: string;
  slug: string;
  name: string;
}

export interface RawConversationRecord {
  vaults?: string[];
  id: string;
  slug: string;
  name: string;
  latest_event_id?: string | null;
}

export interface RawTurnRecord {
  id: string;
  session_id: string;
}

export interface RawArtifactVersion {
  artifact_id: string;
  path: string;
  version: number;
  created_at: string;
  size_bytes: number;
}

export interface RawArtifact extends RawArtifactVersion {
  contents: number[];
}

export type RawSecret =
  | {
      type: "key";
      value: string;
    }
  | {
      type: "github_cli";
      value: string;
      account: string;
    }
  | {
      type: "oauth";
      access_token: string;
      refresh_token?: string | null;
      expires_at?: number | null;
      refresh?: {
        token_endpoint: string;
        client_id: string;
        client_secret?: string | null;
        client_secret_basic?: boolean;
        resource: string | null;
        scopes: string[];
      } | null;
    };

export interface RawVaultRecord {
  id: string;
  name: string;
  created_at: string;
}
export interface RawCredentialPolicy {
  networking:
    | { type: "limited"; allowed_hosts: string[] }
    | { type: "destinations"; allowed_destinations: CredentialDestination[] };
  injection_location: { header: boolean };
}

export interface RawSecretMetadata {
  policy?: RawCredentialPolicy | null;
  revision: number;
  id: string;
  type: "key" | "oauth" | "github_cli";
  name: string;
  created_at: string;
}

export interface RawConversationHandleInfo {
  agent_id: string;
  record: RawConversationRecord;
}

export interface RawTurnHandleInfo {
  conversation: RawConversationHandleInfo;
  record: RawTurnRecord;
}

export interface RawGetEventsResult {
  events: RawEvent[];
  cursor?: string | null;
}

export interface RawAddEventsResult {
  event_ids: string[];
  latest_event_id: string;
}

export interface RawEvent {
  id: string;
  conversation_id: string;
  session_id?: string | null;
  turn_id?: string | null;
  created_at: string;
  data: EventData;
}

export interface RawTypeScriptInitPayload {
  mcp_servers: NativeMcpServer[];
  tools: ToolDefinition[];
  agent: RawAgentRecord;
  conversation: RawConversationHandleInfo;
  turn: RawTurnHandleInfo;
  agent_config: RawAgentConfig;
  conversation_config: RawConversationConfig;
  request: RawSendRequest;
  streaming: boolean;
  recovering: boolean;
  braintrust_parent?: string | null;
}

export type RawRuntimeRequest =
  | { type: "authorize_tool"; request: RawToolRequest }
  | { type: "execute_tool"; request: RawToolRequest; tool_call_id?: string }
  | {
      type: "start_sandbox_process";
      command: string[];
      env: Record<string, string>;
      reuse_key?: string | null;
    }
  | { type: "write_sandbox_process_stdin"; process_id: number; data: string }
  | { type: "close_sandbox_process_stdin"; process_id: number }
  | { type: "close_sandbox_process"; process_id: number };

export type RawRuntimeResponsePayload =
  | { type: "tool_result"; result: ToolResult }
  | {
      type: "sandbox_process_started";
      process_id: number;
      sandbox_id?: string | null;
      sandbox_process_id?: string | null;
      reused?: boolean | null;
    }
  | { type: "unit" };

export type RawSandboxProcessStream = "stdout" | "stderr";

export type RawRuntimeEvent =
  | {
      type: "sandbox_process_output";
      process_id: number;
      stream: RawSandboxProcessStream;
      data: string;
    }
  | {
      type: "sandbox_process_exit";
      process_id: number;
      exit_code?: number | null;
    }
  | {
      type: "sandbox_process_error";
      process_id: number;
      message: string;
    };

export type RawResourceScope =
  | { type: "global" }
  | { type: "agent"; agent_id: string }
  | { type: "thread"; agent_id: string; thread_id: string };

export type RawExoRequest =
  | { type: "list_vaults"; scope: RawResourceScope }
  | { type: "get_vault"; scope: RawResourceScope; vault_id: string }
  | { type: "create_vault"; name: string }
  | { type: "delete_vault"; vault_id: string }
  | { type: "vault_list_secrets"; scope: RawResourceScope; vault_id: string }
  | {
      type: "vault_get_secret";
      scope: RawResourceScope;
      vault_id: string;
      secret_id: string;
    }
  | {
      type: "vault_put_secret";
      scope: RawResourceScope;
      vault_id: string;
      request: {
        name: string;
        secret: RawSecret;
        policy?: RawCredentialPolicy;
      };
    }
  | {
      type: "vault_update_secret";
      scope: RawResourceScope;
      vault_id: string;
      secret_id: string;
      secret?: RawSecret;
      policy?: RawCredentialPolicy;
    }
  | {
      type: "vault_delete_secret";
      scope: RawResourceScope;
      vault_id: string;
      secret_id: string;
    }
  | {
      type: "vault_resolve_secret";
      scope: RawResourceScope;
      vault_id: string;
      secret_id: string;
      target: CredentialDestination;
    }
  | { type: "list_agents" }
  | { type: "get_agent"; agent_id: string }
  | {
      type: "new_agent";
      request: { slug: string; name: string; vaults?: string[] };
    }
  | { type: "delete_agent"; agent_id: string }
  | { type: "list_conversations"; agent_id: string }
  | { type: "get_conversation"; agent_id: string; conversation_id: string }
  | {
      type: "new_conversation";
      agent_id: string;
      request: {
        slug?: string | null;
        name?: string | null;
        vaults?: string[];
      };
    }
  | { type: "delete_conversation"; agent_id: string; conversation_id: string }
  | { type: "agent_list_artifacts"; agent_id: string }
  | {
      type: "agent_read_artifact";
      agent_id: string;
      request: { artifact_id: string; version?: number };
    }
  | {
      type: "agent_write_artifact";
      agent_id: string;
      request: { path: string; contents: number[] };
    }
  | {
      type: "conversation_start_session";
      agent_id: string;
      conversation_id: string;
    }
  | {
      type: "conversation_end_session";
      agent_id: string;
      conversation_id: string;
      session_id: string;
    }
  | {
      type: "conversation_get_events";
      agent_id: string;
      conversation_id: string;
      query?: {
        cursor?: string | null;
        direction?: "asc" | "desc" | null;
        limit?: number | null;
        session_id?: string | null;
        turn_id?: string | null;
        types?: string[] | null;
      } | null;
    }
  | {
      type: "conversation_get_event";
      agent_id: string;
      conversation_id: string;
      event_id: string;
    }
  | {
      type: "conversation_add_events";
      agent_id: string;
      conversation_id: string;
      request: {
        session_id?: string | null;
        turn_id?: string | null;
        data: EventData[];
      };
    }
  | {
      type: "conversation_fork";
      agent_id: string;
      conversation_id: string;
      request: {
        up_to_inclusive?: string | null;
        slug?: string | null;
        name?: string | null;
      };
    }
  | {
      type: "conversation_list_artifacts";
      agent_id: string;
      conversation_id: string;
    }
  | {
      type: "conversation_read_artifact";
      agent_id: string;
      conversation_id: string;
      request: { artifact_id: string; version?: number };
    }
  | {
      type: "conversation_write_artifact";
      agent_id: string;
      conversation_id: string;
      request: { path: string; contents: number[] };
    }
  | {
      type: "turn_add_events";
      agent_id: string;
      conversation_id: string;
      session_id: string;
      turn_id: string;
      data: EventData[];
    }
  | {
      type: "turn_write_artifact";
      agent_id: string;
      conversation_id: string;
      session_id: string;
      turn_id: string;
      request: { path: string; contents: number[] };
    }
  | {
      type: "turn_finish";
      agent_id: string;
      conversation_id: string;
      session_id: string;
      turn_id: string;
    };

export type RawExoResponse =
  | { type: "agents"; agents: RawAgentRecord[] }
  | { type: "agent"; agent: RawAgentRecord | null }
  | { type: "bool"; value: boolean }
  | { type: "conversations"; conversations: RawConversationHandleInfo[] }
  | { type: "conversation"; conversation: RawConversationHandleInfo | null }
  | { type: "events"; result: RawGetEventsResult }
  | { type: "event"; event: RawEvent | null }
  | { type: "add_events"; result: RawAddEventsResult }
  | { type: "session_id"; session_id: string }
  | { type: "artifact_versions"; artifacts: RawArtifactVersion[] }
  | { type: "artifact"; artifact: RawArtifact | null }
  | { type: "artifact_version"; artifact: RawArtifactVersion }
  | { type: "resolved_secret"; secret: RawSecret; revision: number }
  | { type: "vault"; vault: RawVaultRecord | null }
  | { type: "vaults"; vaults: RawVaultRecord[] }
  | { type: "secret_metadata"; metadata: RawSecretMetadata }
  | { type: "secret_id"; secret_id: string }
  | { type: "secrets"; secrets: RawSecretMetadata[] }
  | { type: "secret"; secret: RawSecret | null }
  | { type: "turn"; turn: RawTurnHandleInfo }
  | { type: "event_id"; event_id: string }
  | { type: "unit" };

export type HostToGuestMessage =
  | { kind: "init"; payload: RawTypeScriptInitPayload }
  | { kind: "shutdown" }
  | {
      kind: "runtime_response";
      id: number;
      ok: boolean;
      payload?: RawRuntimeResponsePayload | null;
      error?: string | null;
    }
  | {
      kind: "exo_response";
      id: number;
      ok: boolean;
      response?: RawExoResponse | null;
      error?: string | null;
    }
  | { kind: "runtime_event"; event: RawRuntimeEvent };

export type GuestToHostMessage =
  | { kind: "runtime_request"; id: number; request: RawRuntimeRequest }
  | { kind: "exo_request"; id: number; request: RawExoRequest }
  | { kind: "stream_event"; event: RawTypeScriptStreamEvent }
  | { kind: "done" }
  | { kind: "error"; message: string; stack?: string | null };

export type RawTypeScriptStreamEvent =
  | { type: "first_chunk"; ttft_ms: number }
  | { type: "text_delta"; text: string }
  | {
      type: "tool_call";
      tool_call_id: string;
      tool_name: string;
      arguments: JsonObject;
    }
  | { type: "tool_result"; tool_call_id: string; result: ToolResult };

export interface HarnessClient {
  requestExo(request: RawExoRequest): Promise<RawExoResponse>;
  requestRuntime(
    request: RawRuntimeRequest,
  ): Promise<RawRuntimeResponsePayload>;
  startSandboxProcess(
    request: SandboxProcessStartRequest,
  ): Promise<SandboxProcess>;
  emitStream(event: RawTypeScriptStreamEvent): Promise<void>;
}

function toRawToolRequest(request: ToolRequest): RawToolRequest {
  return {
    function_name: request.functionName,
    arguments: request.arguments,
  };
}

function toAgentRecord(raw: RawAgentRecord): AgentRecord {
  return {
    vaults: raw.vaults ?? [],
    id: raw.id,
    slug: raw.slug,
    name: raw.name,
  };
}

function toConversationRecord(raw: RawConversationRecord): ConversationRecord {
  return {
    vaults: raw.vaults ?? [],
    id: raw.id,
    slug: raw.slug,
    name: raw.name,
    latestEventId: raw.latest_event_id ?? null,
  };
}

function toTurnRecord(raw: RawTurnRecord): TurnRecord {
  return {
    id: raw.id,
    sessionId: raw.session_id,
  };
}

function toArtifactVersion(raw: RawArtifactVersion): ArtifactVersion {
  return {
    artifactId: raw.artifact_id,
    path: raw.path,
    version: raw.version,
    createdAt: raw.created_at,
    sizeBytes: raw.size_bytes,
  };
}

function toArtifact(raw: RawArtifact): Artifact {
  return {
    ...toArtifactVersion(raw),
    contents: Uint8Array.from(raw.contents),
  };
}

function toSecretMetadata(raw: RawSecretMetadata): SecretMetadata {
  return {
    revision: raw.revision,
    policy: raw.policy ? toCredentialPolicy(raw.policy) : null,
    id: raw.id,
    type: raw.type,
    name: raw.name,
    createdAt: raw.created_at,
  };
}

function toSecret(raw: RawSecret): Secret {
  if (raw.type === "key" || raw.type === "github_cli") {
    return raw;
  }
  return {
    type: "oauth",
    accessToken: raw.access_token,
    refreshToken: raw.refresh_token ?? null,
    expiresAt: raw.expires_at ?? null,
    refresh: raw.refresh
      ? {
          tokenEndpoint: raw.refresh.token_endpoint,
          clientId: raw.refresh.client_id,
          clientSecret: raw.refresh.client_secret,
          clientSecretBasic: raw.refresh.client_secret_basic,
          resource: raw.refresh.resource,
          scopes: raw.refresh.scopes,
        }
      : null,
  };
}

function toRawCredentialPolicy(policy: CredentialPolicy): RawCredentialPolicy {
  return {
    networking:
      policy.networking.type === "limited"
        ? { type: "limited", allowed_hosts: policy.networking.allowedHosts }
        : {
            type: "destinations",
            allowed_destinations: policy.networking.allowedDestinations,
          },
    injection_location: policy.injectionLocation,
  };
}

function toCredentialPolicy(policy: RawCredentialPolicy): CredentialPolicy {
  return {
    networking:
      policy.networking.type === "limited"
        ? { type: "limited", allowedHosts: policy.networking.allowed_hosts }
        : {
            type: "destinations",
            allowedDestinations: policy.networking.allowed_destinations,
          },
    injectionLocation: policy.injection_location,
  };
}

function decodeArtifactText(artifact: Artifact | null): string | null {
  if (!artifact) {
    return null;
  }
  return new TextDecoder().decode(artifact.contents);
}

function decodeArtifactJson<T>(artifact: Artifact | null): T | null {
  const text = decodeArtifactText(artifact);
  if (text === null) {
    return null;
  }
  return JSON.parse(text) as T;
}

function toEvent(raw: RawEvent): Event {
  return {
    id: raw.id,
    conversationId: raw.conversation_id,
    sessionId: raw.session_id ?? null,
    turnId: raw.turn_id ?? null,
    createdAt: raw.created_at,
    data: raw.data,
  };
}

function toGetEventsResult(raw: RawGetEventsResult): GetEventsResult {
  return {
    events: raw.events.map(toEvent),
    cursor: raw.cursor ?? null,
  };
}

function toAddEventsResult(raw: RawAddEventsResult): AddEventsResult {
  return {
    eventIds: raw.event_ids,
    latestEventId: raw.latest_event_id,
  };
}

type RawEventQuery = {
  cursor?: string | null;
  direction?: "asc" | "desc" | null;
  limit?: number | null;
  session_id?: string | null;
  turn_id?: string | null;
  types?: string[] | null;
};

function toRawEventQuery(query?: EventQuery): RawEventQuery | null {
  if (!query) {
    return null;
  }
  return {
    cursor: query.cursor ?? null,
    direction: query.direction ?? null,
    limit: query.limit ?? null,
    session_id: query.sessionId ?? null,
    turn_id: query.turnId ?? null,
    types: query.types ?? null,
  };
}

function toRawAddEventsRequest(request: AddEventsRequest): {
  session_id?: string | null;
  turn_id?: string | null;
  data: EventData[];
} {
  return {
    session_id: request.sessionId ?? null,
    turn_id: request.turnId ?? null,
    data: request.data,
  };
}

function toRawNewConversationRequest(request?: NewConversationRequest): {
  vaults?: string[];
  slug?: string | null;
  name?: string | null;
} {
  return {
    slug: request?.slug ?? null,
    name: request?.name ?? null,
    vaults: request?.vaults,
  };
}

function toRawForkConversationRequest(request?: ForkConversationRequest): {
  up_to_inclusive?: string | null;
  slug?: string | null;
  name?: string | null;
} {
  return {
    up_to_inclusive: request?.upToInclusive ?? null,
    slug: request?.slug ?? null,
    name: request?.name ?? null,
  };
}

function createAgent(client: HarnessClient, raw: RawAgentRecord): Agent {
  const record = toAgentRecord(raw);
  const agent: Agent = {
    ...createVaultContext(client, { type: "agent", agent_id: record.id }),
    record,

    async listConversations(): Promise<Conversation[]> {
      const payload = await client.requestExo({
        type: "list_conversations",
        agent_id: record.id,
      });
      if (payload.type !== "conversations") {
        throw new Error(`expected conversations payload, got ${payload.type}`);
      }
      return payload.conversations.map((conversation) =>
        createConversation(client, conversation),
      );
    },

    async getConversation(id: string): Promise<Conversation | null> {
      const payload = await client.requestExo({
        type: "get_conversation",
        agent_id: record.id,
        conversation_id: id,
      });
      if (payload.type !== "conversation") {
        throw new Error(`expected conversation payload, got ${payload.type}`);
      }
      return payload.conversation
        ? createConversation(client, payload.conversation)
        : null;
    },

    async newConversation(
      request?: NewConversationRequest,
    ): Promise<Conversation> {
      const payload = await client.requestExo({
        type: "new_conversation",
        agent_id: record.id,
        request: toRawNewConversationRequest(request),
      });
      if (payload.type !== "conversation" || !payload.conversation) {
        throw new Error(`expected conversation payload, got ${payload.type}`);
      }
      return createConversation(client, payload.conversation);
    },

    async deleteConversation(id: string): Promise<boolean> {
      const payload = await client.requestExo({
        type: "delete_conversation",
        agent_id: record.id,
        conversation_id: id,
      });
      if (payload.type !== "bool") {
        throw new Error(`expected bool payload, got ${payload.type}`);
      }
      return payload.value;
    },

    async listArtifacts(): Promise<ArtifactVersion[]> {
      const payload = await client.requestExo({
        type: "agent_list_artifacts",
        agent_id: record.id,
      });
      if (payload.type !== "artifact_versions") {
        throw new Error(
          `expected artifact_versions payload, got ${payload.type}`,
        );
      }
      return payload.artifacts.map(toArtifactVersion);
    },

    async readArtifact(args): Promise<Artifact | null> {
      const payload = await client.requestExo({
        type: "agent_read_artifact",
        agent_id: record.id,
        request: {
          artifact_id: args.artifactId,
          version: args.version,
        },
      });
      if (payload.type !== "artifact") {
        throw new Error(`expected artifact payload, got ${payload.type}`);
      }
      return payload.artifact ? toArtifact(payload.artifact) : null;
    },

    async readArtifactText(args): Promise<string | null> {
      return decodeArtifactText(await agent.readArtifact(args));
    },

    async readArtifactJson<T>(args: {
      artifactId: string;
      version?: number;
    }): Promise<T | null> {
      return decodeArtifactJson<T>(await agent.readArtifact(args));
    },

    async writeArtifact(args): Promise<ArtifactVersion> {
      const payload = await client.requestExo({
        type: "agent_write_artifact",
        agent_id: record.id,
        request: {
          path: args.path,
          contents: Array.from(asBytes(args.contents)),
        },
      });
      if (payload.type !== "artifact_version") {
        throw new Error(
          `expected artifact_version payload, got ${payload.type}`,
        );
      }
      return toArtifactVersion(payload.artifact);
    },

    async writeArtifactText(args): Promise<ArtifactVersion> {
      return agent.writeArtifact({
        path: args.path,
        contents: args.text,
      });
    },

    async writeArtifactJson(args): Promise<ArtifactVersion> {
      return agent.writeArtifact({
        path: args.path,
        contents: JSON.stringify(args.value, null, 2),
      });
    },
  };
  return agent;
}

export function createExoHarness(
  client: HarnessClient,
  current?: ExoHarnessCurrent,
): ExoHarness {
  return {
    ...createVaultContext(client, { type: "global" }),
    get current() {
      if (!current) throw new Error("no active turn");
      return current;
    },

    async listAgents(): Promise<Agent[]> {
      const payload = await client.requestExo({ type: "list_agents" });
      if (payload.type !== "agents") {
        throw new Error(`expected agents payload, got ${payload.type}`);
      }
      return payload.agents.map((agent) => createAgent(client, agent));
    },

    async getAgent(id: string): Promise<Agent | null> {
      const payload = await client.requestExo({
        type: "get_agent",
        agent_id: id,
      });
      if (payload.type !== "agent") {
        throw new Error(`expected agent payload, got ${payload.type}`);
      }
      return payload.agent ? createAgent(client, payload.agent) : null;
    },

    async newAgent(request): Promise<Agent> {
      const payload = await client.requestExo({
        type: "new_agent",
        request,
      });
      if (payload.type !== "agent" || !payload.agent) {
        throw new Error(`expected agent payload, got ${payload.type}`);
      }
      return createAgent(client, payload.agent);
    },

    async deleteAgent(id: string): Promise<boolean> {
      const payload = await client.requestExo({
        type: "delete_agent",
        agent_id: id,
      });
      if (payload.type !== "bool") {
        throw new Error(`expected bool payload, got ${payload.type}`);
      }
      return payload.value;
    },

    async createVault(name: string): Promise<Vault> {
      const payload = await client.requestExo({ type: "create_vault", name });
      if (payload.type !== "vault" || !payload.vault) {
        throw new Error("server did not return the new vault");
      }
      return createVault(client, payload.vault, { type: "global" });
    },
    async deleteVault(id: string): Promise<void> {
      const payload = await client.requestExo({
        type: "delete_vault",
        vault_id: id,
      });
      if (payload.type !== "bool" || !payload.value) {
        throw new Error("vault deletion failed");
      }
    },
  };
}

function createConversation(
  client: HarnessClient,
  raw: RawConversationHandleInfo,
): Conversation {
  const record = toConversationRecord(raw.record);
  const conversation: Conversation = {
    agentId: raw.agent_id,
    record,
    ...createVaultContext(client, {
      type: "thread",
      agent_id: raw.agent_id,
      thread_id: record.id,
    }),

    async startSession(): Promise<string> {
      const payload = await client.requestExo({
        type: "conversation_start_session",
        agent_id: raw.agent_id,
        conversation_id: record.id,
      });
      if (payload.type !== "session_id") {
        throw new Error(`expected session_id payload, got ${payload.type}`);
      }
      return payload.session_id;
    },

    async endSession(id: string): Promise<void> {
      const payload = await client.requestExo({
        type: "conversation_end_session",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        session_id: id,
      });
      if (payload.type !== "unit") {
        throw new Error(`expected unit payload, got ${payload.type}`);
      }
    },

    async getEvents(query?: EventQuery): Promise<GetEventsResult> {
      const payload = await client.requestExo({
        type: "conversation_get_events",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        query: toRawEventQuery(query),
      });
      if (payload.type !== "events") {
        throw new Error(`expected events payload, got ${payload.type}`);
      }
      return toGetEventsResult(payload.result);
    },

    async getEvent(id: string): Promise<Event | null> {
      const payload = await client.requestExo({
        type: "conversation_get_event",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        event_id: id,
      });
      if (payload.type !== "event") {
        throw new Error(`expected event payload, got ${payload.type}`);
      }
      return payload.event ? toEvent(payload.event) : null;
    },

    async addEvents(request: AddEventsRequest): Promise<AddEventsResult> {
      const payload = await client.requestExo({
        type: "conversation_add_events",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: toRawAddEventsRequest(request),
      });
      if (payload.type !== "add_events") {
        throw new Error(`expected add_events payload, got ${payload.type}`);
      }
      return toAddEventsResult(payload.result);
    },

    async fork(request?: ForkConversationRequest): Promise<Conversation> {
      const payload = await client.requestExo({
        type: "conversation_fork",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: toRawForkConversationRequest(request),
      });
      if (payload.type !== "conversation" || !payload.conversation) {
        throw new Error(`expected conversation payload, got ${payload.type}`);
      }
      return createConversation(client, payload.conversation);
    },

    async listArtifacts(): Promise<ArtifactVersion[]> {
      const payload = await client.requestExo({
        type: "conversation_list_artifacts",
        agent_id: raw.agent_id,
        conversation_id: record.id,
      });
      if (payload.type !== "artifact_versions") {
        throw new Error(
          `expected artifact_versions payload, got ${payload.type}`,
        );
      }
      return payload.artifacts.map(toArtifactVersion);
    },

    async readArtifact(args): Promise<Artifact | null> {
      const payload = await client.requestExo({
        type: "conversation_read_artifact",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: {
          artifact_id: args.artifactId,
          version: args.version,
        },
      });
      if (payload.type !== "artifact") {
        throw new Error(`expected artifact payload, got ${payload.type}`);
      }
      return payload.artifact ? toArtifact(payload.artifact) : null;
    },

    async readArtifactText(args): Promise<string | null> {
      return decodeArtifactText(await conversation.readArtifact(args));
    },

    async readArtifactJson<T>(args: {
      artifactId: string;
      version?: number;
    }): Promise<T | null> {
      return decodeArtifactJson<T>(await conversation.readArtifact(args));
    },

    async writeArtifact(args): Promise<ArtifactVersion> {
      const payload = await client.requestExo({
        type: "conversation_write_artifact",
        agent_id: raw.agent_id,
        conversation_id: record.id,
        request: {
          path: args.path,
          contents: Array.from(asBytes(args.contents)),
        },
      });
      if (payload.type !== "artifact_version") {
        throw new Error(
          `expected artifact_version payload, got ${payload.type}`,
        );
      }
      return toArtifactVersion(payload.artifact);
    },

    async writeArtifactText(args): Promise<ArtifactVersion> {
      return conversation.writeArtifact({
        path: args.path,
        contents: args.text,
      });
    },

    async writeArtifactJson(args): Promise<ArtifactVersion> {
      return conversation.writeArtifact({
        path: args.path,
        contents: JSON.stringify(args.value, null, 2),
      });
    },
  };
  return conversation;
}

function createTurn(
  client: HarnessClient,
  raw: RawTurnHandleInfo,
  conversation: Conversation,
): Turn {
  const record = toTurnRecord(raw.record);
  const turn: Turn = {
    agentId: raw.conversation.agent_id,
    conversationId: raw.conversation.record.id,
    sessionId: record.sessionId,
    turnId: record.id,
    conversation,
    record,

    async addEvents(data): Promise<AddEventsResult> {
      const payload = await client.requestExo({
        type: "turn_add_events",
        agent_id: raw.conversation.agent_id,
        conversation_id: raw.conversation.record.id,
        session_id: record.sessionId,
        turn_id: record.id,
        data,
      });
      if (payload.type !== "add_events") {
        throw new Error(`expected add_events payload, got ${payload.type}`);
      }
      return toAddEventsResult(payload.result);
    },

    async writeArtifact(args): Promise<ArtifactVersion> {
      const payload = await client.requestExo({
        type: "turn_write_artifact",
        agent_id: raw.conversation.agent_id,
        conversation_id: raw.conversation.record.id,
        session_id: record.sessionId,
        turn_id: record.id,
        request: {
          path: args.path,
          contents: Array.from(asBytes(args.contents)),
        },
      });
      if (payload.type !== "artifact_version") {
        throw new Error(
          `expected artifact_version payload, got ${payload.type}`,
        );
      }
      return toArtifactVersion(payload.artifact);
    },

    async writeArtifactText(args): Promise<ArtifactVersion> {
      return turn.writeArtifact({
        path: args.path,
        contents: args.text,
      });
    },

    async writeArtifactJson(args): Promise<ArtifactVersion> {
      return turn.writeArtifact({
        path: args.path,
        contents: JSON.stringify(args.value, null, 2),
      });
    },
  };
  return turn;
}

export function createTurnContext(
  client: HarnessClient,
  init: RawTypeScriptInitPayload,
): TurnContext {
  const agentConfig = toAgentConfig(init.agent_config);
  const conversationConfig = toConversationConfig(init.conversation_config);
  const request = toSendRequest(init.request);
  const streaming = init.streaming;
  const agent = createAgent(client, init.agent);
  const conversation = createConversation(client, init.conversation);
  const turn = createTurn(client, init.turn, conversation);
  const exoharness = createExoHarness(client, {
    agent,
    conversation,
    turn,
  });

  const context: TurnContext = {
    tools: init.tools,
    mcpServers: init.mcp_servers,
    agentConfig,
    conversationConfig,
    request,
    streaming,
    braintrustParent: init.braintrust_parent ?? null,
    exoharness,
    async authorizeTool(request): Promise<void> {
      await client.requestRuntime({
        type: "authorize_tool",
        request: toRawToolRequest(request),
      });
    },
    async executeTool(request, toolCallId): Promise<ToolResult> {
      const payload = await client.requestRuntime({
        type: "execute_tool",
        request: toRawToolRequest(request),
        tool_call_id: toolCallId,
      });
      if (payload.type !== "tool_result") {
        throw new Error(`expected tool_result payload, got ${payload.type}`);
      }
      return payload.result;
    },

    async startSandboxProcess(request): Promise<SandboxProcess> {
      return client.startSandboxProcess(request);
    },

    async executePendingTools(
      toolCalls: PendingToolCall[],
    ): Promise<EventData[]> {
      const events: EventData[] = [];
      for (const toolCall of toolCalls) {
        let result: ToolResult;
        try {
          result = await context.executeTool(
            toolCall.request,
            toolCall.toolCallId,
          );
        } catch (error) {
          result = {
            ok: false,
            error: runnerErrorMessage(error),
          };
        }
        events.push(toolResultEvent(toolCall.toolCallId, result));
      }
      return events;
    },

    stream: {
      async firstChunk(ttftMs): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "first_chunk",
          ttft_ms: ttftMs,
        });
      },

      async text(text): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "text_delta",
          text,
        });
      },

      async toolCall(args): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "tool_call",
          tool_call_id: args.toolCallId,
          tool_name: args.toolName,
          arguments: args.arguments,
        });
      },

      async toolResult(args): Promise<void> {
        if (!streaming) {
          return;
        }
        await client.emitStream({
          type: "tool_result",
          tool_call_id: args.toolCallId,
          result: args.result,
        });
      },
    },
  };
  return context;
}

function runnerErrorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function toRawSecret(secret: Secret): RawSecret {
  return secret.type === "key" || secret.type === "github_cli"
    ? secret
    : {
        type: "oauth",
        access_token: secret.accessToken,
        refresh_token: secret.refreshToken,
        expires_at: secret.expiresAt,
        refresh: secret.refresh
          ? {
              token_endpoint: secret.refresh.tokenEndpoint,
              client_id: secret.refresh.clientId,
              client_secret: secret.refresh.clientSecret,
              client_secret_basic: secret.refresh.clientSecretBasic,
              resource: secret.refresh.resource,
              scopes: secret.refresh.scopes,
            }
          : null,
      };
}

function createVault(
  client: HarnessClient,
  raw: RawVaultRecord,
  scope: RawResourceScope,
): Vault {
  return {
    record: { id: raw.id, name: raw.name, createdAt: raw.created_at },
    async listSecrets() {
      const payload = await client.requestExo({
        type: "vault_list_secrets",
        scope,
        vault_id: raw.id,
      });
      if (payload.type !== "secrets") {
        throw new Error(`expected secrets payload, got ${payload.type}`);
      }
      return payload.secrets.map(toSecretMetadata);
    },
    async getSecret(id) {
      const payload = await client.requestExo({
        type: "vault_get_secret",
        scope,
        vault_id: raw.id,
        secret_id: id,
      });
      if (payload.type !== "secret") {
        throw new Error(`expected secret payload, got ${payload.type}`);
      }
      return payload.secret ? toSecret(payload.secret) : null;
    },
    async putSecret(request) {
      if (!request?.secret) {
        throw new Error("putSecret requires { name, secret, policy? }");
      }
      const payload = await client.requestExo({
        type: "vault_put_secret",
        scope,
        vault_id: raw.id,
        request: {
          name: request.name,
          secret: toRawSecret(request.secret),
          policy: request.policy
            ? toRawCredentialPolicy(request.policy)
            : undefined,
        },
      });
      if (payload.type !== "secret_id") {
        throw new Error(`expected secret_id payload, got ${payload.type}`);
      }
      return payload.secret_id;
    },
    async resolveSecret(id, target) {
      const payload = await client.requestExo({
        type: "vault_resolve_secret",
        scope,
        vault_id: raw.id,
        secret_id: id,
        target,
      });
      if (payload.type !== "resolved_secret") {
        throw new Error(
          `expected resolved_secret payload, got ${payload.type}`,
        );
      }
      return { secret: toSecret(payload.secret), revision: payload.revision };
    },
    async updateSecret(id, request) {
      if (!request?.secret && !request?.policy) {
        throw new Error(
          "updateSecret requires { secret?, policy? } with at least one field",
        );
      }
      const payload = await client.requestExo({
        type: "vault_update_secret",
        scope,
        vault_id: raw.id,
        secret_id: id,
        secret: request.secret ? toRawSecret(request.secret) : undefined,
        policy: request.policy
          ? toRawCredentialPolicy(request.policy)
          : undefined,
      });
      if (payload.type !== "secret_metadata") {
        throw new Error(
          `expected secret_metadata payload, got ${payload.type}`,
        );
      }
      return toSecretMetadata(payload.metadata);
    },
    async deleteSecret(id) {
      const payload = await client.requestExo({
        type: "vault_delete_secret",
        scope,
        vault_id: raw.id,
        secret_id: id,
      });
      if (payload.type !== "bool" || !payload.value) {
        throw new Error("secret deletion failed");
      }
    },
  };
}

function createVaultContext(
  client: HarnessClient,
  scope: RawResourceScope,
): VaultContext {
  return {
    async listVaults() {
      const payload = await client.requestExo({ type: "list_vaults", scope });
      if (payload.type !== "vaults") {
        throw new Error(`expected vaults payload, got ${payload.type}`);
      }
      return payload.vaults.map((record) => createVault(client, record, scope));
    },
    async getVault(id) {
      const payload = await client.requestExo({
        type: "get_vault",
        scope,
        vault_id: id,
      });
      if (payload.type !== "vault") {
        throw new Error(`expected vault payload, got ${payload.type}`);
      }
      return payload.vault ? createVault(client, payload.vault, scope) : null;
    },
  };
}
