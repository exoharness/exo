import type {
  AgentConfig,
  ConversationConfig,
  FileSystemMount,
  Message,
  PermissionPolicy,
  SendRequest,
} from "./index";

export interface RawAgentConfig {
  instructions: Message[];
  harness: "basic" | "rlm" | "typescript" | "type_script" | "exo";
  typescript?: {
    module_path: string;
    tool_module_paths?: string[];
  } | null;
  enable_agent_tool_creation?: boolean;
  sandbox: {
    image?: string | null;
    provider: AgentConfig["sandbox"]["provider"];
    mounts?: RawConversationConfig["mounts"] | null;
    enable_networking: boolean;
    scope: "agent" | "conversation";
  };
  model: string;
  credential?: string | null;
  base_url?: string | null;
  reasoning_effort?: string | null;
  max_output_tokens?: number | null;
  max_tool_round_trips?: number | null;
  braintrust?: unknown;
}

export interface RawConversationConfig {
  permissions: {
    permission_policy: PermissionPolicy;
    tool_policies: Record<string, PermissionPolicy>;
  };
  environment?: {
    config: {
      default_workdir?: string;
    };
  };
  sandbox_image?: string | null;
  sandbox_provider?: AgentConfig["sandbox"]["provider"] | null;
  shell_program?: string | null;
  sandbox_scope?: "agent" | "conversation" | null;
  mounts: Array<{
    host_path: string;
    mount_path: string;
    mode: "ro" | "rw";
    internal?: boolean | null;
  }>;
}

export interface RawSendRequest {
  input: Message[];
  session_id?: string | null;
}

export function toAgentConfig(raw: RawAgentConfig): AgentConfig {
  return {
    instructions: raw.instructions,
    harness: raw.harness === "type_script" ? "typescript" : raw.harness,
    typescript: raw.typescript
      ? {
          modulePath: raw.typescript.module_path,
          toolModulePaths: raw.typescript.tool_module_paths ?? [],
        }
      : null,
    enableAgentToolCreation: raw.enable_agent_tool_creation ?? false,
    sandbox: {
      image: raw.sandbox.image ?? null,
      provider: raw.sandbox.provider,
      mounts: (raw.sandbox.mounts ?? []).map(toFileSystemMount),
      enableNetworking: raw.sandbox.enable_networking,
      scope: raw.sandbox.scope,
    },
    model: raw.model,
    credential: raw.credential,
    baseUrl: raw.base_url,
    reasoningEffort: raw.reasoning_effort ?? null,
    maxOutputTokens: raw.max_output_tokens ?? null,
    maxToolRoundTrips: raw.max_tool_round_trips ?? null,
    braintrust: raw.braintrust,
  };
}

export function toConversationConfig(
  raw: RawConversationConfig,
): ConversationConfig {
  return {
    permissionPolicy: raw.permissions.permission_policy,
    toolPolicies: raw.permissions.tool_policies,
    workdir: raw.environment?.config.default_workdir,
    sandboxImage: raw.sandbox_image ?? null,
    sandboxProvider: raw.sandbox_provider ?? null,
    shellProgram: raw.shell_program ?? null,
    sandboxScope: raw.sandbox_scope ?? null,
    mounts: raw.mounts.map(toFileSystemMount),
  };
}

export function toFileSystemMount(
  raw: RawConversationConfig["mounts"][number],
): FileSystemMount {
  return {
    hostPath: raw.host_path,
    mountPath: raw.mount_path,
    mode: raw.mode,
    internal: raw.internal ?? null,
  };
}

export function toSendRequest(raw: RawSendRequest): SendRequest {
  return {
    input: raw.input,
    sessionId: raw.session_id ?? null,
  };
}
