#[cfg(feature = "native")]
mod native;
#[cfg(feature = "native")]
pub use native::*;

mod basic;
pub mod conversation_sandbox;
pub mod execution_tracing;
mod executor_types;
mod frontend_tools;
pub use exoharness::harness;
mod harness_adapter;
mod harness_config;
mod harness_events;
mod harness_executor;
mod harness_helpers;
mod harness_types;
pub mod managed_agents;
mod mcp_types;
mod message_history;
mod model_config;
mod model_events;
mod model_execution;
pub mod permissions;
mod provider;
mod turn_queue;
pub use exoharness::runtime_host;
pub use exoharness::turn_coordinator::TurnOptions;
pub use harness_executor::TurnWork;
pub mod sandbox_policy;
mod shared;

pub use executor_types::{
    AgentConfig, AgentHarnessKind, AgentSandboxConfig, ConversationConfig,
    ConversationHarnessConfig, ConversationModelConfig, ExecutionStreamEvent,
    ExecutionStreamHandle, ModelClient, ModelRequest, ModelResponse, ModelResponseStream,
    PendingToolCall, SandboxScope, SendRequest, SendResult, ShellToolArguments, ShellToolResult,
    ToolDefinition, ToolRuntime, TypeScriptHarnessConfig, TypeScriptStreamEvent,
    effective_sandbox_scope, to_execution_stream_event,
};
pub use exo_managed_agents::{BraintrustProject, BraintrustTracingConfig};

#[cfg(feature = "firecracker")]
pub use exoharness::{
    DEFAULT_FIRECRACKER_BINARY, DEFAULT_FIRECRACKER_INITRAMFS, DEFAULT_FIRECRACKER_JAILER,
    DEFAULT_FIRECRACKER_KERNEL, DEFAULT_FIRECRACKER_STATE_ROOT, DEFAULT_IMAGE_SIZE_GIB,
    DEFAULT_JAILER_UID_BASE, DEFAULT_MEMORY_MIB, DEFAULT_NETWORK_BYTES_PER_SECOND,
    DEFAULT_VCPU_COUNT, DEFAULT_WORKSPACE_SIZE_GIB, FirecrackerConfig, FirecrackerLimaConfig,
    run_firecracker_bridge,
};
pub use harness_config::{find_agent_config, load_agent_config, load_conversation_config};
pub use harness_executor::{ExecutorStreamMode, HarnessExecutor, Runtime};
pub use harness_helpers::{
    get_conversation_model_override, materialize_conversation_messages,
    put_conversation_model_override,
};
pub use harness_types::{CreateAgentRequest, CreateConversationRequest};
pub use mcp_types::{NativeMcpServer, NativeMcpTool};
pub use provider::{LocalProvider, Provider, ProviderTurn};

pub use basic::BasicExecutor;

pub use exoharness::EgressPolicy;

pub use exoharness::{
    AgentHandle, AttachSandboxRequest, Binding, BindingRecord, ConversationHandle,
    CreateSandboxRequest, DurableFileSystem, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, ExoHarness, FileSystemMount, FileSystemMountMode, ForkConversationRequest,
    NewAgentRequest, PutSecretRequest, RunInSandboxRequest, SandboxAttachment, SandboxId,
    SandboxProcess, SandboxProvider, SandboxProviderConfig, SandboxRecord, SandboxResourceShape,
    Secret, SecretMetadata, SessionId, SnapshotId, StartSandboxRequest, ToolRequest, TurnId,
    UsageRecord, Uuid7,
};

pub mod shell_tool;
pub mod typescript_runtime;

#[cfg(feature = "native")]
pub mod local_net;
#[cfg(feature = "native")]
pub mod previews;
#[cfg(feature = "native")]
pub use previews::{BrowserPreview, PreviewEndpoint, PreviewUrls, previews_for};
