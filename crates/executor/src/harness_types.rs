use exoharness::SandboxProvider;

use crate::{AgentHarnessKind, BraintrustTracingConfig, SandboxScope, TypeScriptHarnessConfig};

#[derive(Debug, Clone)]
pub struct CreateAgentRequest {
    pub slug: String,
    pub name: Option<String>,
    pub harness: AgentHarnessKind,
    pub typescript: Option<TypeScriptHarnessConfig>,
    pub enable_agent_tool_creation: bool,
    pub sandbox_image: Option<String>,
    pub sandbox_provider: SandboxProvider,
    pub sandbox_scope: Option<SandboxScope>,
    pub enable_networking: bool,
    pub model: String,
    pub max_output_tokens: Option<i64>,
    pub max_tool_round_trips: Option<u32>,
    pub braintrust: Option<BraintrustTracingConfig>,
}

#[derive(Debug, Clone, Default)]
pub struct CreateConversationRequest {
    pub vaults: Vec<exoharness::vault::VaultId>,
    pub slug: Option<String>,
    pub name: Option<String>,
    pub sandbox_image: Option<String>,
    pub sandbox_provider: Option<SandboxProvider>,
    pub shell_program: Option<String>,
}
