use std::time::Duration;

use async_trait::async_trait;
use exoharness::{
    AgentRecord, ConversationRecord, EventId, Result, SessionId, ToolRequest, ToolResult, TurnId,
};

use crate::{AgentConfig, BraintrustTracingConfig, ModelRequest, ModelResponse};

#[async_trait]
pub(crate) trait ExecutionTracer: Send + Sync {
    async fn flush(&self) -> Result<()>;

    async fn start_turn(
        &self,
        config: Option<&BraintrustTracingConfig>,
        agent: &AgentRecord,
        conversation: &ConversationRecord,
        agent_config: &AgentConfig,
        session_id: SessionId,
        turn_id: TurnId,
        streamed: bool,
    ) -> Option<Box<dyn TurnExecutionTrace>>;
}

#[async_trait]
pub(crate) trait TurnExecutionTrace: Send + Sync {
    fn export_parent(&self) -> Option<String> {
        None
    }

    async fn start_llm_round(
        &self,
        request: &ModelRequest,
        round_index: usize,
    ) -> Option<Box<dyn LlmExecutionTrace>>;

    async fn start_tool_call(
        &self,
        request: &ToolRequest,
        round_index: usize,
    ) -> Option<Box<dyn ToolExecutionTrace>>;

    async fn finish_success(self: Box<Self>, latest_event_id: Option<EventId>);

    async fn finish_error(self: Box<Self>, error: &anyhow::Error);
}

#[async_trait]
pub(crate) trait LlmExecutionTrace: Send + Sync {
    async fn finish_success(self: Box<Self>, response: &ModelResponse, ttft: Option<Duration>);

    async fn finish_error(self: Box<Self>, error: &anyhow::Error);
}

#[async_trait]
pub(crate) trait ToolExecutionTrace: Send + Sync {
    async fn finish_success(self: Box<Self>, result: &ToolResult);

    async fn finish_error(self: Box<Self>, error: &anyhow::Error);
}
