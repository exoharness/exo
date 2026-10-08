use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use cost::PricingTable;
use exoharness::{
    AgentHandle, ConversationHandle, ConversationId, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, Result, ToolCallId, ToolRequest, TurnHandle,
};
use lingua::Message;
use serde::{Deserialize, Serialize};
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

pub(crate) const BASIC_TOOL_ROUND: &str = "exo.basic.tool_round";

#[derive(Serialize, Deserialize)]
pub(crate) struct BasicToolRound {
    pub(crate) round: u32,
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
    round: u32,
    resume_first_approval: bool,
    stream_mode: ExecutorStreamMode<'a>,
    turn_trace: Option<&'a dyn TurnExecutionTrace>,
}

struct TurnProgress {
    start_round: u32,
    pending_tool_requests: Option<Vec<ExecutableToolRequest>>,
    resume_first_approval: bool,
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
                    EventKind::custom(crate::frontend_tools::FRONTEND_TOOL_RESPONSE),
                ]),
            }))
            .await?;

        let mut event_messages = cached_entry
            .as_ref()
            .map_or_else(Vec::new, |entry| entry.messages.clone());
        let mut tool_call_names = cached_entry
            .as_ref()
            .map_or_else(HashMap::new, |entry| entry.tool_call_names.clone());
        extend_message_history(&mut event_messages, &mut tool_call_names, &result.events)?;
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
        recovering: bool,
    ) -> Result<()> {
        let TurnProgress {
            start_round,
            mut pending_tool_requests,
            resume_first_approval,
        } = if recovering {
            self.load_turn_progress(conversation, turn.as_ref(), agent_config)
                .await?
        } else {
            TurnProgress {
                start_round: 0,
                pending_tool_requests: None,
                resume_first_approval: false,
            }
        };
        for round in start_round.. {
            let (tool_requests, resume_first_approval) = if let Some(saved_requests) =
                pending_tool_requests.take()
            {
                // This model round was already committed; finish its tools before
                // applying the limit to the next model call.
                (saved_requests, resume_first_approval)
            } else {
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
                request
                    .tools
                    .extend(crate::frontend_tools::definitions(agent_config));
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
                events.insert(
                    0,
                    EventData::Custom {
                        event_type: BASIC_TOOL_ROUND.to_owned(),
                        payload: serde_json::to_value(BasicToolRound { round })?,
                    },
                );
                turn.add_events(events).await?;
                (tool_requests, false)
            };

            self.execute_tool_round(
                ToolRoundContext {
                    agent,
                    conversation,
                    turn: Arc::clone(&turn),
                    agent_config,
                    conversation_config,
                    round,
                    resume_first_approval,
                    stream_mode,
                    turn_trace,
                },
                tool_requests,
            )
            .await?;
        }

        Ok(())
    }

    async fn execute_tool_round(
        &self,
        context: ToolRoundContext<'_>,
        tool_requests: Vec<ExecutableToolRequest>,
    ) -> Result<()> {
        for (index, tool_request) in tool_requests.into_iter().enumerate() {
            let mut tool_trace = match context.turn_trace {
                Some(turn_trace) => {
                    turn_trace
                        .start_tool_call(&tool_request.request, context.round as usize)
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
                    Some(&tool_request.tool_call_id),
                    Some(context.round),
                    context.resume_first_approval && index == 0,
                    &tool_request.request,
                    context.stream_mode,
                )
                .await?;
                if crate::frontend_tools::contains(
                    context.agent_config,
                    &tool_request.request.function_name,
                ) {
                    return crate::frontend_tools::execute(
                        context.conversation,
                        context.turn.as_ref(),
                        &tool_request.tool_call_id,
                        &tool_request.request,
                    )
                    .await;
                }
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
            context
                .turn
                .add_events(vec![EventData::ToolResult {
                    tool_call_id: tool_request.tool_call_id,
                    result,
                }])
                .await?;
        }

        Ok(())
    }

    async fn load_turn_progress(
        &self,
        conversation: &dyn ConversationHandle,
        turn: &dyn TurnHandle,
        agent_config: &AgentConfig,
    ) -> Result<TurnProgress> {
        // Rebuild the saved model round before entering the normal turn loop.
        // Calling the model again could produce a different set of tool calls.
        let events = conversation
            .get_events(Some(EventQuery {
                turn_id: Some(turn.record().id),
                session_id: Some(turn.record().session_id),
                direction: Some(EventQueryDirection::Asc),
                types: Some(vec![
                    EventKind::custom(BASIC_TOOL_ROUND),
                    EventKind::TOOL_REQUESTED,
                    EventKind::TOOL_RESULT,
                ]),
                ..Default::default()
            }))
            .await?
            .events;
        let mut round = None;
        let mut pending = Vec::new();
        for event in events {
            match event.data {
                EventData::Custom {
                    event_type,
                    payload,
                } if event_type == BASIC_TOOL_ROUND => {
                    anyhow::ensure!(pending.is_empty(), "previous tool round is incomplete");
                    round = Some(serde_json::from_value::<BasicToolRound>(payload)?.round);
                }
                EventData::ToolRequested {
                    tool_call_id,
                    request,
                    ..
                } => {
                    anyhow::ensure!(round.is_some(), "tool call has no saved round");
                    pending.push(ExecutableToolRequest {
                        tool_call_id,
                        request,
                    });
                }
                EventData::ToolResult { tool_call_id, .. } => {
                    if let Some(index) = pending
                        .iter()
                        .position(|request| request.tool_call_id == tool_call_id)
                    {
                        pending.remove(index);
                    }
                }
                _ => {}
            }
        }
        let next_round = round.map_or(0, |round| round + 1);
        if pending.is_empty() {
            return Ok(TurnProgress {
                start_round: next_round,
                pending_tool_requests: None,
                resume_first_approval: false,
            });
        }
        let first = &pending[0];
        let round = round.expect("pending tool call has a saved round");
        let approvals = crate::permissions::approval_events(
            conversation,
            EventQuery {
                turn_id: Some(turn.record().id),
                session_id: Some(turn.record().session_id),
                ..Default::default()
            },
        )
        .await?;
        let mut resume_first_approval = false;
        for event in &approvals {
            if let EventData::Custom {
                event_type,
                payload,
            } = &event.data
                && event_type == crate::permissions::APPROVAL_REQUESTED
            {
                let approval: crate::permissions::ApprovalRequest =
                    serde_json::from_value(payload.clone())?;
                resume_first_approval |= approval.tool_call_id.as_deref()
                    == Some(first.tool_call_id.as_str())
                    && approval.round == Some(round)
                    && approval.request == first.request;
            }
        }
        anyhow::ensure!(
            crate::frontend_tools::contains(agent_config, &first.request.function_name)
                || crate::permissions::pending_from_events(approvals)?
                    .iter()
                    .any(|approval| approval.tool_call_id.as_deref()
                        == Some(first.tool_call_id.as_str())
                        && approval.round == Some(round)
                        && approval.request == first.request),
            "cannot safely resume unresolved tool call `{}` (`{}`) for turn {}",
            first.tool_call_id,
            first.request.function_name,
            turn.record().id
        );
        Ok(TurnProgress {
            start_round: round,
            pending_tool_requests: Some(pending),
            resume_first_approval,
        })
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

    fn can_resume_pending_approval(&self, _config: &AgentConfig) -> bool {
        true
    }

    fn can_suspend_turn(&self, _config: &AgentConfig) -> bool {
        true
    }

    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
    ) -> Result<()> {
        let mut definitions = build_tool_definitions(conversation_config);
        definitions.extend(self.tools.definitions());
        crate::frontend_tools::validate(agent_config, &definitions)?;
        definitions.extend(crate::frontend_tools::definitions(agent_config));
        conversation_config
            .permissions
            .validate_tool_names(definitions.iter().map(|tool| tool.name.as_str()))?;
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
            false,
        )
        .await
    }

    async fn resume_turn(
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
            true,
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
