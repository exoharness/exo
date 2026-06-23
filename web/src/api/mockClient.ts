import { EVENT_PAGE_SIZE } from "./exoClient";
import {
  MOCK_AGENT_BINDINGS,
  MOCK_AGENT_SECRETS,
  MOCK_AGENTS,
  MOCK_ARTIFACTS,
  MOCK_CONVERSATION_BINDINGS,
  MOCK_CONVERSATION_SECRETS,
  MOCK_CONVERSATIONS,
  MOCK_EVENTS_BY_CONVERSATION,
  MOCK_ROOT_BINDINGS,
  MOCK_ROOT_SECRETS,
} from "./mockData";
import type {
  AgentId,
  AgentRecord,
  Artifact,
  ArtifactId,
  BindingRecord,
  ConversationHandleInfo,
  ConversationId,
  EventId,
  GetEventsResult,
  SecretMetadata,
} from "./protocol";

export class MockClient {
  private readonly artifactCache = new Map<string, Promise<Artifact>>();

  async health(_signal?: AbortSignal): Promise<string> {
    return "ok (demo mode)";
  }

  async listAgents(): Promise<AgentRecord[]> {
    return [...MOCK_AGENTS];
  }

  async listConversations(agentId: AgentId): Promise<ConversationHandleInfo[]> {
    return MOCK_CONVERSATIONS.filter(
      (conversation) => conversation.agent_id === agentId,
    );
  }

  async getEventsPage(
    _agentId: AgentId,
    conversationId: ConversationId,
    cursor: EventId | null,
  ): Promise<GetEventsResult> {
    const all = [...(MOCK_EVENTS_BY_CONVERSATION[conversationId] ?? [])].sort(
      (left, right) => left.id.localeCompare(right.id),
    );

    let start = 0;
    if (cursor) {
      const cursorIndex = all.findIndex((event) => event.id === cursor);
      start = cursorIndex === -1 ? all.length : cursorIndex + 1;
    }

    const page = all.slice(start, start + EVENT_PAGE_SIZE);
    const nextCursor =
      page.length > 0 ? page[page.length - 1]!.id : (cursor ?? null);

    return {
      events: page,
      cursor: page.length > 0 ? nextCursor : null,
    };
  }

  async readConversationArtifact(
    _agentId: AgentId,
    _conversationId: ConversationId,
    artifactId: ArtifactId,
    version?: number | null,
  ): Promise<Artifact> {
    const key = `${artifactId}:${version ?? "latest"}`;
    const cached = this.artifactCache.get(key);
    if (cached) {
      return cached;
    }

    const pending = Promise.resolve().then(() => {
      const artifact = MOCK_ARTIFACTS[artifactId];
      if (!artifact) {
        throw new Error("artifact not found");
      }
      if (version != null && artifact.version !== version) {
        throw new Error("artifact version not found");
      }
      return artifact;
    });

    pending.catch(() => this.artifactCache.delete(key));
    this.artifactCache.set(key, pending);
    return pending;
  }

  async listRootSecrets(): Promise<SecretMetadata[]> {
    return [...MOCK_ROOT_SECRETS];
  }

  async listAgentSecrets(agentId: AgentId): Promise<SecretMetadata[]> {
    return [...(MOCK_AGENT_SECRETS[agentId] ?? [])];
  }

  async listConversationSecrets(
    _agentId: AgentId,
    conversationId: ConversationId,
  ): Promise<SecretMetadata[]> {
    return [...(MOCK_CONVERSATION_SECRETS[conversationId] ?? [])];
  }

  async listRootBindings(): Promise<BindingRecord[]> {
    return [...MOCK_ROOT_BINDINGS];
  }

  async listAgentBindings(agentId: AgentId): Promise<BindingRecord[]> {
    return [...(MOCK_AGENT_BINDINGS[agentId] ?? [])];
  }

  async listConversationBindings(
    _agentId: AgentId,
    conversationId: ConversationId,
  ): Promise<BindingRecord[]> {
    return [...(MOCK_CONVERSATION_BINDINGS[conversationId] ?? [])];
  }
}

export const mockClient = new MockClient();
