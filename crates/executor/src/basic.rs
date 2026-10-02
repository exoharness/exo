use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cost::PricingTable;
use exoharness::{
    AgentHandle, ConversationHandle, ConversationId, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, Result, ToolCallId, ToolRequest, TurnHandle,
};
use lingua::Message;
use serde_json::json;

use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorStreamMode, HarnessExecutor};
use crate::harness_helpers::resolve_model;
use crate::message_history::extend_message_history;
use crate::model_events::model_response_events;
use crate::model_execution::{ModelStreamOutput, complete_model_round};
use crate::shared::HISTORY_CACHE_NAME;
use crate::{
    AgentConfig, ConversationConfig, ModelClient, ModelRequest, SendRequest, ToolDefinition,
    ToolRuntime,
};

pub struct BasicExecutor<M, T> {
    model: Arc<M>,
    tools: Arc<T>,
    history_cache: Arc<RwLock<HashMap<ConversationId, HistoryCacheEntry>>>,
    pricing: Arc<PricingTable>,
}

impl<M, T> BasicExecutor<M, T> {
    #[cfg(test)]
    pub fn new(model: Arc<M>, tools: Arc<T>) -> Self {
        Self::with_pricing(model, tools, Arc::new(PricingTable::empty()))
    }

    /// Cost is filled from `pricing`; an empty table leaves `cost_usd` unset.
    pub fn with_pricing(model: Arc<M>, tools: Arc<T>, pricing: Arc<PricingTable>) -> Self {
        Self {
            model,
            tools,
            history_cache: Arc::new(RwLock::new(HashMap::new())),
            pricing,
        }
    }
}

struct ToolRoundContext<'a> {
    agent: &'a dyn AgentHandle,
    conversation: &'a dyn ConversationHandle,
    turn: Arc<dyn TurnHandle>,
    agent_config: &'a AgentConfig,
    conversation_config: &'a ConversationConfig,
    round: usize,
    stream_mode: ExecutorStreamMode<'a>,
    turn_trace: Option<&'a dyn TurnExecutionTrace>,
}

impl<M, T> BasicExecutor<M, T>
where
    M: ModelClient + 'static,
    T: ToolRuntime + 'static,
{
    async fn materialize_prompt_history(
        &self,
        conversation: &dyn ConversationHandle,
        instructions: &[Message],
    ) -> Result<Vec<Message>> {
        let conversation_id = conversation.record().id;
        let cached_entry = {
            let cache = self.history_cache.read().expect(HISTORY_CACHE_NAME);
            cache.get(&conversation_id).cloned()
        };

        let result = conversation
            .get_events(Some(EventQuery {
                cursor: cached_entry.as_ref().and_then(|entry| entry.cursor),
                direction: Some(EventQueryDirection::Asc),
                limit: None,
                session_id: None,
                turn_id: None,
                types: Some(vec![
                    EventKind::MESSAGES,
                    EventKind::TOOL_REQUESTED,
                    EventKind::TOOL_RESULT,
                ]),
            }))
            .await?;

        let mut event_messages = cached_entry
            .as_ref()
            .map_or_else(Vec::new, |entry| entry.messages.clone());
        let mut tool_call_names = cached_entry
            .as_ref()
            .map_or_else(HashMap::new, |entry| entry.tool_call_names.clone());
        extend_message_history(&mut event_messages, &mut tool_call_names, &result.events);
        let cursor = result
            .cursor
            .or_else(|| cached_entry.and_then(|entry| entry.cursor));

        self.history_cache
            .write()
            .expect(HISTORY_CACHE_NAME)
            .insert(
                conversation_id,
                HistoryCacheEntry {
                    cursor,
                    messages: event_messages.clone(),
                    tool_call_names,
                },
            );

        let mut messages = instructions.to_vec();
        messages.extend(event_messages);
        Ok(messages)
    }

    async fn run_turn_loop(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        for round in 0u32.. {
            if agent_config
                .max_tool_round_trips
                .is_some_and(|limit| round > limit)
            {
                return Ok(());
            }

            let messages = self
                .materialize_prompt_history(conversation, &agent_config.instructions)
                .await?;
            let mut request =
                build_model_request(conversation, agent_config, conversation_config, messages)
                    .await?;
            request.tools.extend(self.tools.definitions());
            let response = complete_model_round(
                self.model.as_ref(),
                request,
                round as usize,
                stream_mode,
                ModelStreamOutput::Visible,
                turn_trace,
            )
            .await?;

            let mut events = model_response_events(response, &self.pricing);
            let tool_requests = collect_tool_requests(&events);
            if tool_requests.is_empty() {
                // Commit the final answer and completion marker together for recovery.
                events.push(EventData::Custom {
                    event_type: crate::harness_executor::RUNTIME_TURN_COMPLETED.to_owned(),
                    payload: serde_json::Value::Null,
                });
                turn.add_events(events).await?;
                return Ok(());
            }
            turn.add_events(events).await?;

            let tool_results = self
                .execute_tool_round(
                    ToolRoundContext {
                        agent,
                        conversation,
                        turn: Arc::clone(&turn),
                        agent_config,
                        conversation_config,
                        round: round as usize,
                        stream_mode,
                        turn_trace,
                    },
                    tool_requests,
                )
                .await?;
            turn.add_events(tool_results).await?;
        }

        Ok(())
    }

    async fn execute_tool_round(
        &self,
        context: ToolRoundContext<'_>,
        tool_requests: Vec<ExecutableToolRequest>,
    ) -> Result<Vec<EventData>> {
        let mut tool_results = Vec::with_capacity(tool_requests.len());

        for tool_request in tool_requests {
            let mut tool_trace = match context.turn_trace {
                Some(turn_trace) => {
                    turn_trace
                        .start_tool_call(&tool_request.request, context.round)
                        .await
                }
                None => None,
            };
            let tool_future = async {
                crate::permissions::authorize(
                    context.conversation,
                    context.turn.as_ref(),
                    self.tools.permission_policy(
                        &context.conversation_config.permissions,
                        &tool_request.request.function_name,
                    ),
                    &tool_request.request,
                    context.stream_mode,
                )
                .await?;
                self.tools
                    .execute(
                        context.agent,
                        context.conversation,
                        Some(context.turn.as_ref()),
                        context.agent_config,
                        context.conversation_config,
                        &tool_request.request,
                    )
                    .await
            };
            let (result, tool_succeeded) = match tool_future.await {
                Ok(response) => (response, true),
                Err(error) => {
                    if let Some(tool_trace) = tool_trace.take() {
                        tool_trace.finish_error(&error).await;
                    }
                    (
                        json!({
                            "ok": false,
                            "error": format!("{error:#}"),
                        }),
                        false,
                    )
                }
            };
            if tool_succeeded && let Some(tool_trace) = tool_trace.take() {
                tool_trace.finish_success(&result).await;
            }
            tool_results.push(EventData::ToolResult {
                tool_call_id: tool_request.tool_call_id,
                result,
            });
        }

        Ok(tool_results)
    }
}

#[async_trait]
impl<M, T> HarnessExecutor for BasicExecutor<M, T>
where
    M: ModelClient + 'static,
    T: ToolRuntime + 'static,
{
    fn name(&self) -> &'static str {
        "basic"
    }

    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
    ) -> Result<()> {
        conversation_config.permissions.validate_tool_names(
            build_tool_definitions(conversation_config)
                .iter()
                .chain(self.tools.definitions().iter())
                .map(|tool| tool.name.as_str()),
        )?;
        self.tools
            .prepare_conversation(agent, conversation, agent_config, conversation_config)
            .await
    }

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: Arc<dyn ConversationHandle>,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        _request: &SendRequest,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        self.run_turn_loop(
            agent,
            conversation.as_ref(),
            turn,
            agent_config,
            conversation_config,
            stream_mode,
            turn_trace,
        )
        .await
    }
}

#[derive(Debug, Clone)]
struct ExecutableToolRequest {
    tool_call_id: String,
    request: ToolRequest,
}

fn collect_tool_requests(events: &[EventData]) -> Vec<ExecutableToolRequest> {
    events
        .iter()
        .filter_map(|event| match event {
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } => Some(ExecutableToolRequest {
                tool_call_id: tool_call_id.clone(),
                request: request.clone(),
            }),
            _ => None,
        })
        .collect()
}

async fn build_model_request(
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
    messages: Vec<Message>,
) -> Result<ModelRequest> {
    let model_binding = resolve_model(conversation, agent_config).await?;
    Ok(ModelRequest {
        model: model_binding.model,
        api_key: Some(model_binding.api_key),
        base_url: model_binding.base_url,
        messages,
        tools: build_tool_definitions(conversation_config),
        max_output_tokens: agent_config.max_output_tokens,
    })
}

fn build_tool_definitions(config: &ConversationConfig) -> Vec<ToolDefinition> {
    let mut tools = Vec::new();

    if let Some(program) = &config.shell_program {
        tools.push(ToolDefinition {
            strict: None,
            name: "shell".to_string(),
            description: format!("Run a shell command using {program}."),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        });
    }

    tools
}

#[derive(Debug, Clone, Default)]
struct HistoryCacheEntry {
    cursor: Option<EventId>,
    messages: Vec<Message>,
    tool_call_names: HashMap<ToolCallId, String>,
}
