import type {
  AgentId,
  Artifact,
  ArtifactId,
  ArtifactVersion,
  AgentRecord,
  BindingRecord,
  ConversationHandleInfo,
  ConversationId,
  EventId,
  ExoResponse,
  GetEventsResult,
  ReadOnlyRequest,
  SecretMetadata,
  ServerMessage,
} from "./protocol";

const DEFAULT_PAGE_SIZE = 100;
export const EVENT_PAGE_SIZE = 500;

export class ExoClient {
  private readonly requestEndpoint: string;
  private readonly healthEndpoint: string;
  private nextRequestId = 1;
  // Artifacts are immutable per (id, version), so a fetched payload can be cached
  // for the life of this client (a new base URL constructs a fresh client + cache).
  private readonly artifactCache = new Map<string, Promise<Artifact>>();

  constructor(baseUrl: string) {
    this.requestEndpoint = normalizeRequestEndpoint(baseUrl);
    this.healthEndpoint = normalizeHealthEndpoint(baseUrl);
  }

  async health(signal?: AbortSignal): Promise<string> {
    const response = await fetch(this.healthEndpoint, {
      method: "GET",
      headers: { accept: "text/plain" },
      signal,
    });
    if (!response.ok) {
      throw new Error(`GET /health failed (${response.status})`);
    }
    return response.text();
  }

  async listAgents(): Promise<AgentRecord[]> {
    const response = await this.send({ type: "list_agents" }, "agents");
    return response.agents;
  }

  async listConversations(agentId: AgentId): Promise<ConversationHandleInfo[]> {
    const conversations: ConversationHandleInfo[] = [];
    let cursor: EventId | null = null;

    for (;;) {
      const response: Extract<ExoResponse, { type: "conversations" }> =
        await this.send(
          {
            type: "list_conversations",
            agent_id: agentId,
            request: { cursor, limit: DEFAULT_PAGE_SIZE },
          },
          "conversations",
        );
      conversations.push(...response.result.conversations);
      cursor = response.result.next_cursor;
      if (!cursor) {
        return conversations;
      }
    }
  }

  // One page of events strictly after `cursor` (an event id) in ascending order.
  // This is the single substrate read the live poller and the full loader share;
  // a future `watch_events` endpoint would replace the polling, not this call.
  async getEventsPage(
    agentId: AgentId,
    conversationId: ConversationId,
    cursor: EventId | null,
  ): Promise<GetEventsResult> {
    const response = await this.send(
      {
        type: "conversation_get_events",
        agent_id: agentId,
        conversation_id: conversationId,
        query: {
          cursor,
          direction: "asc",
          limit: EVENT_PAGE_SIZE,
        },
      },
      "events",
    );
    return response.result;
  }

  async readConversationArtifact(
    agentId: AgentId,
    conversationId: ConversationId,
    artifactId: ArtifactId,
    version?: number | null,
  ): Promise<Artifact> {
    const key = `${agentId}:${conversationId}:${artifactId}:${version ?? "latest"}`;
    const cached = this.artifactCache.get(key);
    if (cached) {
      return cached;
    }

    const pending = this.send(
      {
        type: "conversation_read_artifact",
        agent_id: agentId,
        conversation_id: conversationId,
        request: { artifact_id: artifactId, version: version ?? null },
      },
      "artifact",
    ).then((response) => {
      if (!response.artifact) {
        throw new Error("artifact not found");
      }
      return response.artifact;
    });

    // Keep only successful reads cached so a failed fetch can be retried.
    pending.catch(() => this.artifactCache.delete(key));
    this.artifactCache.set(key, pending);
    return pending;
  }

  async listAgentArtifacts(agentId: AgentId): Promise<ArtifactVersion[]> {
    const response = await this.send(
      { type: "agent_list_artifacts", agent_id: agentId },
      "artifact_versions",
    );
    return response.artifacts;
  }

  async readAgentArtifact(
    agentId: AgentId,
    artifactId: ArtifactId,
    version?: number | null,
  ): Promise<Artifact> {
    const key = `agent:${agentId}:${artifactId}:${version ?? "latest"}`;
    const cached = this.artifactCache.get(key);
    if (cached) {
      return cached;
    }

    const pending = this.send(
      {
        type: "agent_read_artifact",
        agent_id: agentId,
        request: { artifact_id: artifactId, version: version ?? null },
      },
      "artifact",
    ).then((response) => {
      if (!response.artifact) {
        throw new Error("artifact not found");
      }
      return response.artifact;
    });

    pending.catch(() => this.artifactCache.delete(key));
    this.artifactCache.set(key, pending);
    return pending;
  }

  async listRootSecrets(): Promise<SecretMetadata[]> {
    const response = await this.send({ type: "list_secrets" }, "secrets");
    return response.secrets;
  }

  async listAgentSecrets(agentId: AgentId): Promise<SecretMetadata[]> {
    const response = await this.send(
      { type: "agent_list_secrets", agent_id: agentId },
      "secrets",
    );
    return response.secrets;
  }

  async listConversationSecrets(
    agentId: AgentId,
    conversationId: ConversationId,
  ): Promise<SecretMetadata[]> {
    const response = await this.send(
      {
        type: "conversation_list_secrets",
        agent_id: agentId,
        conversation_id: conversationId,
      },
      "secrets",
    );
    return response.secrets;
  }

  async listRootBindings(): Promise<BindingRecord[]> {
    const response = await this.send({ type: "list_bindings" }, "bindings");
    return response.bindings;
  }

  async listAgentBindings(agentId: AgentId): Promise<BindingRecord[]> {
    const response = await this.send(
      { type: "agent_list_bindings", agent_id: agentId },
      "bindings",
    );
    return response.bindings;
  }

  async listConversationBindings(
    agentId: AgentId,
    conversationId: ConversationId,
  ): Promise<BindingRecord[]> {
    const response = await this.send(
      {
        type: "conversation_list_bindings",
        agent_id: agentId,
        conversation_id: conversationId,
      },
      "bindings",
    );
    return response.bindings;
  }

  private async send<T extends ExoResponse["type"]>(
    request: ReadOnlyRequest,
    expectedType: T,
  ): Promise<Extract<ExoResponse, { type: T }>> {
    const id = this.nextRequestId;
    this.nextRequestId += 1;

    const response = await fetch(this.requestEndpoint, {
      method: "POST",
      headers: {
        accept: "application/json",
        "content-type": "application/json",
      },
      body: JSON.stringify({
        kind: "request",
        id,
        request,
      }),
    });

    if (!response.ok) {
      const body = await response.text().catch(() => "");
      throw new Error(
        `POST /request failed (${response.status})${body ? `: ${body}` : ""}`,
      );
    }

    const message = (await response.json()) as ServerMessage;
    if (message.kind !== "response") {
      throw new Error("server returned a non-response message");
    }
    if (message.id !== id) {
      throw new Error(
        `response id ${message.id} did not match request id ${id}`,
      );
    }
    if (!message.ok) {
      throw new Error(message.error || "exoharness request failed");
    }
    if (!message.response) {
      throw new Error("server returned ok without a response payload");
    }
    if (message.response.type !== expectedType) {
      throw new Error(
        `expected ${expectedType} response, got ${message.response.type}`,
      );
    }
    return message.response as Extract<ExoResponse, { type: T }>;
  }
}

export function normalizeRequestEndpoint(rawBaseUrl: string): string {
  const url = parseBaseUrl(rawBaseUrl);
  url.search = "";
  url.hash = "";
  const path = stripTrailingSlashes(url.pathname);
  if (path.endsWith("/request")) {
    return url.toString();
  }
  url.pathname = `${path}/request`;
  return url.toString();
}

export function normalizeHealthEndpoint(rawBaseUrl: string): string {
  const url = parseBaseUrl(rawBaseUrl);
  url.search = "";
  url.hash = "";
  const path = stripTrailingSlashes(url.pathname);
  const basePath = path.endsWith("/request")
    ? path.slice(0, -"/request".length)
    : path;
  url.pathname = `${basePath}/health`;
  return url.toString();
}

function parseBaseUrl(rawBaseUrl: string): URL {
  const trimmed = rawBaseUrl.trim();
  if (!trimmed) {
    throw new Error("base URL is empty");
  }

  const hasScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed);
  const candidate =
    hasScheme || trimmed.startsWith("/") ? trimmed : `http://${trimmed}`;
  return new URL(candidate, window.location.origin);
}

function stripTrailingSlashes(pathname: string): string {
  const path = pathname.replace(/\/+$/, "");
  return path === "" ? "" : path;
}
