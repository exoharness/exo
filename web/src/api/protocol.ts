export type JsonPrimitive = string | number | boolean | null;
export type JsonValue =
  | JsonPrimitive
  | JsonValue[]
  | { [key: string]: JsonValue };
export type JsonObject = { [key: string]: JsonValue };

export type AgentId = string;
export type ConversationId = string;
export type SessionId = string;
export type TurnId = string;
export type EventId = string;
export type ResponseId = string;
export type ToolCallId = string;
export type ArtifactId = string;
export type SandboxId = string;
export type SandboxProcessId = string;
export type SnapshotId = string;
export type BindingId = string;
export type SecretId = string;
export type DateTimeUtc = string;

export interface AgentRecord {
  id: AgentId;
  slug: string;
  name: string;
}

export interface NewAgentRequest {
  slug: string;
  name: string;
}

export interface ConversationRecord {
  id: ConversationId;
  slug: string;
  name: string;
  latest_event_id: EventId | null;
}

export interface ConversationHandleInfo {
  agent_id: AgentId;
  record: ConversationRecord;
}

export interface NewConversationRequest {
  slug?: string | null;
  name?: string | null;
}

export interface ListConversationsRequest {
  cursor?: EventId | null;
  limit?: number | null;
}

export interface ListConversationsResult<T> {
  conversations: T[];
  next_cursor: EventId | null;
}

export interface TurnRecord {
  id: TurnId;
  session_id: SessionId;
}

export interface TurnHandleInfo {
  conversation: ConversationHandleInfo;
  record: TurnRecord;
}

export type EventQueryDirection = "asc" | "desc";

export interface EventQuery {
  cursor?: EventId | null;
  direction?: EventQueryDirection | null;
  limit?: number | null;
  session_id?: SessionId | null;
  turn_id?: TurnId | null;
  types?: string[] | null;
}

export interface UsageRecord {
  model: string;
  prompt_tokens?: number | null;
  completion_tokens?: number | null;
  prompt_cached_tokens?: number | null;
  prompt_cache_creation_tokens?: number | null;
  completion_reasoning_tokens?: number | null;
  cost_usd?: number | null;
  ttft_ms?: number | null;
  duration_ms?: number | null;
}

export type UserContent =
  | string
  | Array<{ type: "text"; text: string } | ({ type: string } & JsonObject)>;

export type AssistantContent =
  | string
  | Array<
      | { type: "text"; text: string }
      | ({ type: "reasoning"; text: string } & JsonObject)
      | ({
          type: "tool_call";
          tool_call_id?: ToolCallId | null;
          tool_name: string;
          arguments: JsonValue;
        } & JsonObject)
      | ({
          type: "tool_result";
          tool_call_id?: ToolCallId | null;
          tool_name: string;
          output: JsonValue;
        } & JsonObject)
      | ({ type: "file" } & JsonObject)
      | ({ type: string } & JsonObject)
    >;

export type ToolContent = Array<
  | ({
      type: "tool_result";
      tool_call_id?: ToolCallId | null;
      tool_name: string;
      output: JsonValue;
    } & JsonObject)
  | ({ type: string } & JsonObject)
>;

export type LinguaMessage =
  | { role: "user"; content: UserContent }
  | { role: "assistant"; content: AssistantContent; id?: string | null }
  | { role: "tool"; content: ToolContent }
  | { role: "system"; content: UserContent }
  | { role: "developer"; content: UserContent }
  | ({ role: string; content?: JsonValue } & JsonObject);

export interface Event {
  id: EventId;
  conversation_id: ConversationId;
  session_id: SessionId | null;
  turn_id: TurnId | null;
  created_at: DateTimeUtc;
  data: EventData;
}

export type ToolArguments = JsonObject;
export type ToolResult = JsonValue;

export interface ToolRequest {
  function_name: string;
  arguments: ToolArguments;
}

export type SandboxProvider =
  | "daytona"
  | "vercel"
  | "aws_agentcore"
  | "aws-agentcore"
  | "apple_container"
  | "docker"
  | "local_process"
  | "local";

export type FileSystemMountMode = "ro" | "rw";

export interface FileSystemMount {
  host_path: string;
  mount_path: string;
  mode: FileSystemMountMode;
  internal: boolean | null;
}

export interface DurableFileSystem {
  name: string;
  mount_path: string;
  mode: FileSystemMountMode;
}

export interface CreateSandboxRequest {
  name?: string | null;
  provider: SandboxProvider;
  image: string;
  default_workdir?: string | null;
  file_system_mounts?: FileSystemMount[] | null;
  durable_file_systems?: DurableFileSystem[] | null;
  enable_networking?: boolean | null;
  idle_seconds?: number | null;
}

export interface StartSandboxRequest {
  id: SandboxId;
  snapshot_id: SnapshotId;
  idle_seconds?: number | null;
}

export interface RunInSandboxRequest {
  id: SandboxId;
  command: string[];
  env?: Record<string, string>;
}

export type SandboxProcessMode = "exec" | "pty";
export type SandboxProcessStdin = "none" | "open";
export type SandboxProcessOutput = "buffered" | "stream";
export type SandboxProcessLifecycle = "attached" | "detached";

export interface StartSandboxProcessRequest {
  sandbox_id: SandboxId;
  name?: string | null;
  command: string[];
  env?: Record<string, string>;
  cwd: string | null;
  mode?: SandboxProcessMode;
  stdin?: SandboxProcessStdin;
  output?: SandboxProcessOutput;
  lifecycle?: SandboxProcessLifecycle;
}

export interface SandboxProcessRecord {
  id: SandboxProcessId;
  sandbox_id: SandboxId;
  name?: string | null;
  status: SandboxProcessStatus;
}

export type SandboxProcessStatus =
  | { type: "running" }
  | { type: "exited"; exit_code: number }
  | { type: "failed"; message: string }
  | { type: "cancelled" };

export interface SandboxProcessEventQuery {
  sandbox_id: SandboxId;
  process_id: SandboxProcessId;
  after?: number | null;
  limit?: number | null;
  follow?: boolean | null;
}

export interface GetSandboxProcessEventsResult {
  events: SandboxProcessEvent[];
  cursor: number | null;
  status: SandboxProcessStatus;
}

export type SandboxProcessEvent =
  | { type: "stdout"; cursor: number; data: number[] }
  | { type: "stderr"; cursor: number; data: number[] }
  | { type: "exit"; cursor: number; exit_code: number }
  | { type: "error"; cursor: number; message: string }
  | { type: "cancelled"; cursor: number };

export interface WriteSandboxProcessInputRequest {
  sandbox_id: SandboxId;
  process_id: SandboxProcessId;
  data: number[];
}

export interface CloseSandboxProcessInputRequest {
  sandbox_id: SandboxId;
  process_id: SandboxProcessId;
}

export interface WaitSandboxProcessRequest {
  sandbox_id: SandboxId;
  process_id: SandboxProcessId;
}

export interface CancelSandboxProcessRequest {
  sandbox_id: SandboxId;
  process_id: SandboxProcessId;
  signal?: string | null;
}

export type EventData =
  | { type: "conversation_created"; slug: string; name: string }
  | { type: "conversation_updated"; slug: string | null; name: string | null }
  | { type: "conversation_deleted" }
  | {
      type: "conversation_forked";
      source_conversation_id: ConversationId;
      up_to_inclusive: EventId | null;
    }
  | { type: "session_started" }
  | { type: "session_ended" }
  | { type: "turn_started" }
  | { type: "turn_ended" }
  | {
      type: "messages";
      messages: LinguaMessage[];
      response_id: ResponseId | null;
      usage?: UsageRecord | null;
    }
  | {
      type: "tool_requested";
      tool_call_id: ToolCallId;
      response_id: ResponseId | null;
      request: ToolRequest;
    }
  | { type: "tool_result"; tool_call_id: ToolCallId; result: ToolResult }
  | { type: "lingua_stream_chunk"; chunk: JsonObject }
  | { type: "error"; message: string }
  | {
      type: "artifact_written";
      artifact_id: ArtifactId;
      path: string;
      version: number;
    }
  | {
      type: "sandbox_created";
      sandbox_id: SandboxId;
      name?: string | null;
      provider: SandboxProvider;
      image: string;
      default_workdir: string;
      file_system_mounts: FileSystemMount[];
      durable_file_systems?: DurableFileSystem[] | null;
      enable_networking: boolean;
      idle_seconds: number;
    }
  | {
      type: "sandbox_started";
      sandbox_id: SandboxId;
      snapshot_id: SnapshotId | null;
    }
  | { type: "sandbox_stopped"; sandbox_id: SandboxId }
  | {
      type: "sandbox_snapshotted";
      sandbox_id: SandboxId;
      snapshot_id: SnapshotId;
    }
  | {
      type: "sandbox_process_started";
      sandbox_id: SandboxId;
      process_id: SandboxProcessId;
      name?: string | null;
      command: string[];
      cwd: string | null;
      mode: SandboxProcessMode;
      stdin: SandboxProcessStdin;
      output: SandboxProcessOutput;
      lifecycle: SandboxProcessLifecycle;
      status: SandboxProcessStatus;
      provider_state: JsonValue | null;
    }
  | {
      type: "sandbox_process_state_updated";
      sandbox_id: SandboxId;
      process_id: SandboxProcessId;
      status: SandboxProcessStatus;
      provider_state: JsonValue | null;
    }
  | {
      type: "sandbox_process_event";
      sandbox_id: SandboxId;
      process_id: SandboxProcessId;
      event: SandboxProcessEvent;
    }
  | { type: "custom"; event_type: string; payload: JsonValue };

export interface GetEventsResult {
  events: Event[];
  cursor: EventId | null;
}

export interface AddEventsRequest {
  session_id?: SessionId | null;
  turn_id?: TurnId | null;
  expected_head?: EventId | null;
  data: EventData[];
}

export interface AddEventsResult {
  event_ids: EventId[];
  latest_event_id: EventId;
}

export interface ForkConversationRequest {
  up_to_inclusive?: EventId | null;
  slug?: string | null;
  name?: string | null;
}

export interface ArtifactVersion {
  artifact_id: ArtifactId;
  path: string;
  version: number;
  created_at: DateTimeUtc;
  size_bytes: number;
}

export interface Artifact {
  artifact_id: ArtifactId;
  path: string;
  version: number;
  created_at: DateTimeUtc;
  size_bytes: number;
  contents: number[];
}

export interface WriteArtifactRequest {
  path: string;
  contents: number[];
}

export interface ReadArtifactRequest {
  artifact_id: ArtifactId;
  version?: number | null;
}

export type BindingType = "env" | "mcp" | "llm" | "sandbox";

export type SandboxProviderConfig =
  | { provider: "docker"; default_image: string }
  | {
      provider: "daytona";
      api_key_secret_id: SecretId;
      region?: string | null;
      organization_id?: string | null;
      api_url?: string | null;
      default_image: string;
    }
  | {
      provider: "vercel";
      api_token_secret_id: SecretId;
      team_id: string;
      project_id: string;
      api_url?: string | null;
      default_image: string;
    }
  | {
      provider: "aws_agentcore" | "aws-agentcore";
      runtime_arn: string;
      region: string;
      qualifier?: string | null;
      endpoint_url?: string | null;
      default_image: string;
    };

export type Binding =
  | { type: "env"; name: string; env_var: string; secret_id: SecretId }
  | {
      type: "mcp";
      name: string;
      server_url: string;
      secret_id: SecretId | null;
    }
  | {
      type: "llm";
      name: string;
      model: string;
      base_url: string | null;
      secret_id: SecretId | null;
    }
  | { type: "sandbox"; name: string; config: SandboxProviderConfig };

export interface BindingRecord {
  id: BindingId;
  type: BindingType;
  name: string;
  created_at: DateTimeUtc;
  binding: Binding;
}

export type SecretType = "key" | "oauth";

export interface SecretMetadata {
  id: SecretId;
  type: SecretType;
  name: string;
  created_at: DateTimeUtc;
}

export type Secret =
  | { type: "key"; value: string }
  | { type: "oauth"; access_token: string; refresh_token: string | null };

export interface PutSecretRequest {
  name: string;
  secret: Secret;
}

export type ExoRequest =
  | { type: "list_agents" }
  | { type: "get_agent"; agent_id: AgentId }
  | { type: "new_agent"; request: NewAgentRequest }
  | { type: "delete_agent"; agent_id: AgentId }
  | { type: "list_bindings" }
  | { type: "put_binding"; binding: Binding }
  | { type: "get_binding"; binding_id: BindingId }
  | { type: "list_secrets" }
  | { type: "put_secret"; request: PutSecretRequest }
  | { type: "get_secret"; secret_id: SecretId }
  | {
      type: "list_conversations";
      agent_id: AgentId;
      request: ListConversationsRequest;
    }
  | {
      type: "get_conversation";
      agent_id: AgentId;
      conversation_id: ConversationId;
    }
  | {
      type: "new_conversation";
      agent_id: AgentId;
      request: NewConversationRequest;
    }
  | {
      type: "delete_conversation";
      agent_id: AgentId;
      conversation_id: ConversationId;
    }
  | { type: "agent_list_artifacts"; agent_id: AgentId }
  | {
      type: "agent_read_artifact";
      agent_id: AgentId;
      request: ReadArtifactRequest;
    }
  | {
      type: "agent_write_artifact";
      agent_id: AgentId;
      request: WriteArtifactRequest;
    }
  | { type: "agent_list_bindings"; agent_id: AgentId }
  | { type: "agent_put_binding"; agent_id: AgentId; binding: Binding }
  | { type: "agent_get_binding"; agent_id: AgentId; binding_id: BindingId }
  | { type: "agent_list_secrets"; agent_id: AgentId }
  | { type: "agent_put_secret"; agent_id: AgentId; request: PutSecretRequest }
  | { type: "agent_get_secret"; agent_id: AgentId; secret_id: SecretId }
  | {
      type: "conversation_start_session";
      agent_id: AgentId;
      conversation_id: ConversationId;
    }
  | {
      type: "conversation_end_session";
      agent_id: AgentId;
      conversation_id: ConversationId;
      session_id: SessionId;
    }
  | {
      type: "conversation_begin_turn";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: BeginTurnRequest;
    }
  | {
      type: "conversation_get_events";
      agent_id: AgentId;
      conversation_id: ConversationId;
      query?: EventQuery | null;
    }
  | {
      type: "conversation_get_event";
      agent_id: AgentId;
      conversation_id: ConversationId;
      event_id: EventId;
    }
  | {
      type: "conversation_add_events";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: AddEventsRequest;
    }
  | {
      type: "conversation_fork";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: ForkConversationRequest;
    }
  | {
      type: "conversation_list_artifacts";
      agent_id: AgentId;
      conversation_id: ConversationId;
    }
  | {
      type: "conversation_read_artifact";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: ReadArtifactRequest;
    }
  | {
      type: "conversation_write_artifact";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: WriteArtifactRequest;
    }
  | {
      type: "conversation_create_sandbox";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: CreateSandboxRequest;
    }
  | {
      type: "conversation_snapshot_sandbox";
      agent_id: AgentId;
      conversation_id: ConversationId;
      sandbox_id: SandboxId;
    }
  | {
      type: "conversation_start_sandbox";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: StartSandboxRequest;
    }
  | {
      type: "conversation_stop_sandbox";
      agent_id: AgentId;
      conversation_id: ConversationId;
      sandbox_id: SandboxId;
    }
  | {
      type: "conversation_start_sandbox_process";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: StartSandboxProcessRequest;
    }
  | {
      type: "conversation_write_sandbox_process_input";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: WriteSandboxProcessInputRequest;
    }
  | {
      type: "conversation_close_sandbox_process_input";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: CloseSandboxProcessInputRequest;
    }
  | {
      type: "conversation_get_sandbox_process_events";
      agent_id: AgentId;
      conversation_id: ConversationId;
      query: SandboxProcessEventQuery;
    }
  | {
      type: "conversation_wait_sandbox_process";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: WaitSandboxProcessRequest;
    }
  | {
      type: "conversation_cancel_sandbox_process";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: CancelSandboxProcessRequest;
    }
  | {
      type: "conversation_list_bindings";
      agent_id: AgentId;
      conversation_id: ConversationId;
    }
  | {
      type: "conversation_put_binding";
      agent_id: AgentId;
      conversation_id: ConversationId;
      binding: Binding;
    }
  | {
      type: "conversation_get_binding";
      agent_id: AgentId;
      conversation_id: ConversationId;
      binding_id: BindingId;
    }
  | {
      type: "conversation_list_secrets";
      agent_id: AgentId;
      conversation_id: ConversationId;
    }
  | {
      type: "conversation_put_secret";
      agent_id: AgentId;
      conversation_id: ConversationId;
      request: PutSecretRequest;
    }
  | {
      type: "conversation_get_secret";
      agent_id: AgentId;
      conversation_id: ConversationId;
      secret_id: SecretId;
    }
  | {
      type: "turn_add_events";
      agent_id: AgentId;
      conversation_id: ConversationId;
      session_id: SessionId;
      turn_id: TurnId;
      data: EventData[];
    }
  | {
      type: "turn_write_artifact";
      agent_id: AgentId;
      conversation_id: ConversationId;
      session_id: SessionId;
      turn_id: TurnId;
      request: WriteArtifactRequest;
    }
  | {
      type: "turn_finish";
      agent_id: AgentId;
      conversation_id: ConversationId;
      session_id: SessionId;
      turn_id: TurnId;
    };

export interface BeginTurnRequest {
  session_id?: SessionId | null;
  input: LinguaMessage[];
}

export type ExoResponse =
  | { type: "agents"; agents: AgentRecord[] }
  | { type: "agent"; agent: AgentRecord | null }
  | { type: "bool"; value: boolean }
  | {
      type: "conversations";
      result: ListConversationsResult<ConversationHandleInfo>;
    }
  | { type: "conversation"; conversation: ConversationHandleInfo | null }
  | { type: "events"; result: GetEventsResult }
  | { type: "event"; event: Event | null }
  | { type: "add_events"; result: AddEventsResult }
  | { type: "session_id"; session_id: SessionId }
  | { type: "artifact_versions"; artifacts: ArtifactVersion[] }
  | { type: "artifact"; artifact: Artifact | null }
  | { type: "artifact_version"; artifact: ArtifactVersion }
  | { type: "sandbox_id"; sandbox_id: SandboxId }
  | { type: "snapshot_id"; snapshot_id: SnapshotId }
  | { type: "sandbox_process"; process: SandboxProcessRecord }
  | { type: "sandbox_process_events"; result: GetSandboxProcessEventsResult }
  | { type: "sandbox_process_status"; status: SandboxProcessStatus }
  | { type: "bindings"; bindings: BindingRecord[] }
  | { type: "binding"; binding: Binding | null }
  | { type: "secrets"; secrets: SecretMetadata[] }
  | { type: "secret"; secret: Secret | null }
  | { type: "binding_id"; binding_id: BindingId }
  | { type: "secret_id"; secret_id: SecretId }
  | { type: "turn"; turn: TurnHandleInfo }
  | { type: "event_id"; event_id: EventId }
  | { type: "unit" };

export type ClientMessage = {
  kind: "request";
  id: number;
  request: ExoRequest;
};

export type ServerMessage = {
  kind: "response";
  id: number;
  ok: boolean;
  response: ExoResponse | null;
  error: string | null;
};

export type ReadOnlyRequest = Extract<
  ExoRequest,
  | { type: "list_agents" }
  | { type: "get_agent" }
  | { type: "list_bindings" }
  | { type: "get_binding" }
  | { type: "list_secrets" }
  | { type: "list_conversations" }
  | { type: "get_conversation" }
  | { type: "agent_list_artifacts" }
  | { type: "agent_read_artifact" }
  | { type: "agent_list_bindings" }
  | { type: "agent_get_binding" }
  | { type: "agent_list_secrets" }
  | { type: "conversation_get_events" }
  | { type: "conversation_get_event" }
  | { type: "conversation_list_artifacts" }
  | { type: "conversation_read_artifact" }
  | { type: "conversation_get_sandbox_process_events" }
  | { type: "conversation_list_bindings" }
  | { type: "conversation_get_binding" }
  | { type: "conversation_list_secrets" }
>;
