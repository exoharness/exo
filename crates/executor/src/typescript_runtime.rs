//! Portable request handling for TypeScript harnesses. Hosts only transport
//! runner requests and events; tools and sandbox processes use the shared traits.
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use exoharness::{
    AddEventsRequest, AgentHandle, CancelSandboxProcessRequest, CloseSandboxProcessInputRequest,
    ConversationHandle, EventData, EventKind, EventQuery, EventQueryDirection,
    GetSandboxProcessEventsResult, SandboxId, SandboxProcessEvent, SandboxProcessEventQuery,
    SandboxProcessId, SandboxProcessLifecycle, SandboxProcessMode, SandboxProcessStatus,
    SandboxProcessStdin, StartSandboxProcessRequest, ToolRequest, ToolResult, TurnHandle,
    WriteSandboxProcessInputRequest,
    protocol::{ConversationHandleInfo, TurnHandleInfo},
};
use futures::future::{AbortHandle, Abortable};
use serde::{Deserialize, Serialize};

use crate::runtime_host::RuntimeHost;
use crate::{AgentConfig, ConversationConfig, ExecutorStreamMode, SendRequest, ToolRuntime};

pub struct RequestContext<'a> {
    pub agent: &'a dyn AgentHandle,
    pub conversation: &'a dyn ConversationHandle,
    pub turn: &'a dyn TurnHandle,
    pub agent_config: &'a AgentConfig,
    pub conversation_config: &'a ConversationConfig,
    pub stream: ExecutorStreamMode<'a>,
}

pub struct TypeScriptRuntime {
    thread: Arc<dyn ConversationHandle>,
    tools: Arc<dyn ToolRuntime>,
    host: Arc<dyn RuntimeHost>,
    emit: Arc<dyn Fn(RuntimeEvent) -> Result<()> + Send + Sync>,
    processes: Mutex<HashMap<u64, RunningSandboxProcess>>,
}

struct RunningSandboxProcess {
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
    event_task: AbortHandle,
}
impl Drop for RunningSandboxProcess {
    fn drop(&mut self) {
        self.event_task.abort();
    }
}

static NEXT_PROCESS_ID: AtomicU64 = AtomicU64::new(1);

impl TypeScriptRuntime {
    pub async fn init(
        &self,
        context: &RequestContext<'_>,
        request: &SendRequest,
        recovering: bool,
        braintrust_parent: Option<String>,
    ) -> Result<TypeScriptInitPayload> {
        let conversation = ConversationHandleInfo {
            agent_id: context.agent.record().id,
            record: context.conversation.record().clone(),
        };
        Ok(TypeScriptInitPayload {
            agent: context.agent.record().clone(),
            conversation: conversation.clone(),
            turn: TurnHandleInfo {
                conversation,
                record: context.turn.record().clone(),
            },
            agent_config: context.agent_config.clone(),
            conversation_config: context.conversation_config.clone(),
            request: request.clone(),
            streaming: matches!(context.stream, ExecutorStreamMode::Enabled(_)),
            recovering,
            braintrust_parent,
            tools: self.tools.definitions(),
            mcp_servers: self.tools.mcp_servers(context.conversation).await?,
        })
    }
    pub fn new(
        thread: Arc<dyn ConversationHandle>,
        tools: Arc<dyn ToolRuntime>,
        host: Arc<dyn RuntimeHost>,
        emit: Arc<dyn Fn(RuntimeEvent) -> Result<()> + Send + Sync>,
    ) -> Self {
        Self {
            thread,
            tools,
            host,
            emit,
            processes: Mutex::default(),
        }
    }

    pub async fn shutdown(&self) -> Result<()> {
        let processes = std::mem::take(
            &mut *self
                .processes
                .lock()
                .expect("TypeScript processes poisoned"),
        );
        let results =
            futures::future::join_all(processes.into_values().map(|process| async move {
                process.event_task.abort();
                self.thread
                    .cancel_sandbox_process(CancelSandboxProcessRequest {
                        sandbox_id: process.sandbox_id.clone(),
                        process_id: process.process_id.clone(),
                        signal: None,
                    })
                    .await
            }))
            .await;
        for result in results {
            result?;
        }
        Ok(())
    }

    /// Process I/O can outlive a turn; authorization, execution and process
    /// creation require an active turn context.
    pub async fn handle_request(
        &self,
        context: Option<RequestContext<'_>>,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponsePayload> {
        if let RuntimeRequest::ExecuteTool { request } | RuntimeRequest::AuthorizeTool { request } =
            &request
        {
            let context = context.as_ref().context("TypeScript turn is not active")?;
            crate::permissions::authorize(
                context.conversation,
                context.turn,
                self.tools.permission_policy(
                    &context.conversation_config.permissions,
                    &request.function_name,
                ),
                None,
                None,
                false,
                request,
                context.stream,
            )
            .await?;
        }
        match request {
            RuntimeRequest::AuthorizeTool { .. } => Ok(RuntimeResponsePayload::ToolResult {
                result: serde_json::Value::Null,
            }),
            RuntimeRequest::ExecuteTool { request } => {
                let context = context.context("TypeScript turn is not active")?;
                Ok(RuntimeResponsePayload::ToolResult {
                    result: self
                        .tools
                        .execute(
                            context.agent,
                            context.conversation,
                            Some(context.turn),
                            context.agent_config,
                            context.conversation_config,
                            &request,
                        )
                        .await?,
                })
            }
            RuntimeRequest::StartSandboxProcess {
                command,
                env,
                reuse_key,
            } => {
                let context = context.context("TypeScript turn is not active")?;
                self.start_process(
                    context.conversation,
                    context.agent_config,
                    context.conversation_config,
                    command,
                    env,
                    reuse_key,
                )
                .await
            }
            RuntimeRequest::WriteSandboxProcessStdin { process_id, data } => {
                let (sandbox_id, process_id) = self.process_identity(process_id)?;
                self.thread
                    .write_sandbox_process_input(WriteSandboxProcessInputRequest {
                        sandbox_id,
                        process_id,
                        data: data.into_bytes(),
                    })
                    .await?;
                Ok(RuntimeResponsePayload::Unit)
            }
            RuntimeRequest::CloseSandboxProcessStdin { process_id } => {
                let (sandbox_id, process_id) = self.process_identity(process_id)?;
                self.thread
                    .close_sandbox_process_input(CloseSandboxProcessInputRequest {
                        sandbox_id,
                        process_id,
                    })
                    .await?;
                Ok(RuntimeResponsePayload::Unit)
            }
            RuntimeRequest::CloseSandboxProcess { process_id } => {
                let process = self
                    .processes
                    .lock()
                    .expect("TypeScript processes poisoned")
                    .remove(&process_id);
                if let Some(process) = process {
                    process.event_task.abort();
                    self.thread
                        .cancel_sandbox_process(CancelSandboxProcessRequest {
                            sandbox_id: process.sandbox_id.clone(),
                            process_id: process.process_id.clone(),
                            signal: None,
                        })
                        .await?;
                    (self.emit)(RuntimeEvent::Exit {
                        process_id,
                        exit_code: None,
                    })?;
                }
                Ok(RuntimeResponsePayload::Unit)
            }
        }
    }

    fn process_identity(&self, id: u64) -> Result<(SandboxId, SandboxProcessId)> {
        let processes = self
            .processes
            .lock()
            .expect("TypeScript processes poisoned");
        let process = processes
            .get(&id)
            .ok_or_else(|| anyhow!("sandbox process is not active: {id}"))?;
        Ok((process.sandbox_id.clone(), process.process_id.clone()))
    }

    async fn start_process(
        &self,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        command: Vec<String>,
        env: HashMap<String, String>,
        reuse_key: Option<String>,
    ) -> Result<RuntimeResponsePayload> {
        let sandbox_id = crate::conversation_sandbox::ensure_conversation_sandbox(
            conversation,
            agent_config,
            conversation_config,
            conversation_config.shell_program.as_deref(),
        )
        .await?;
        let reusable_process = match reuse_key.as_deref() {
            Some(reuse_key) => {
                reusable_sandbox_process(conversation, reuse_key, &sandbox_id).await?
            }
            None => None,
        };
        let (sandbox_process_id, reused, cursor) = match reusable_process {
            Some(process) => process,
            None => {
                let process = conversation
                    .start_sandbox_process(StartSandboxProcessRequest {
                        sandbox_id: sandbox_id.clone(),
                        name: None,
                        command,
                        env,
                        cwd: None,
                        mode: SandboxProcessMode::Exec,
                        stdin: SandboxProcessStdin::Open,
                        output: Default::default(),
                        lifecycle: SandboxProcessLifecycle::Attached,
                    })
                    .await?;
                if let Some(reuse_key) = reuse_key {
                    conversation
                        .add_events(AddEventsRequest {
                            session_id: None,
                            turn_id: None,
                            data: vec![EventData::Custom {
                                event_type: TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT.to_string(),
                                payload: serde_json::to_value(
                                    TypeScriptSandboxProcessReuseEvent {
                                        reuse_key,
                                        sandbox_id: process.sandbox_id.clone(),
                                        process_id: process.id.clone(),
                                    },
                                )?,
                            }],
                        })
                        .await?;
                }
                (process.id, false, None)
            }
        };

        let process_id = NEXT_PROCESS_ID.fetch_add(1, Ordering::Relaxed);
        let event_task = spawn_process_events(
            self.host.as_ref(),
            self.emit.clone(),
            self.thread.clone(),
            process_id,
            sandbox_id.clone(),
            sandbox_process_id.clone(),
            cursor,
        );
        self.processes
            .lock()
            .expect("TypeScript processes poisoned")
            .insert(
                process_id,
                RunningSandboxProcess {
                    sandbox_id: sandbox_id.clone(),
                    process_id: sandbox_process_id.clone(),
                    event_task,
                },
            );
        Ok(RuntimeResponsePayload::SandboxProcessStarted {
            process_id,
            sandbox_id,
            sandbox_process_id,
            reused,
        })
    }
}
const TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT: &str = "typescript_sandbox_process_reuse";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypeScriptSandboxProcessReuseEvent {
    reuse_key: String,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
}

async fn reusable_sandbox_process(
    conversation: &dyn ConversationHandle,
    reuse_key: &str,
    desired_sandbox_id: &SandboxId,
) -> Result<Option<(SandboxProcessId, bool, Option<u64>)>> {
    let events = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(100),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::custom(
                TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT,
            )]),
        }))
        .await?
        .events;

    for event in events {
        let EventData::Custom {
            event_type,
            payload,
        } = event.data
        else {
            continue;
        };
        if event_type != TYPESCRIPT_SANDBOX_PROCESS_REUSE_EVENT {
            continue;
        }
        let candidate: TypeScriptSandboxProcessReuseEvent = serde_json::from_value(payload)?;
        if candidate.reuse_key != reuse_key || candidate.sandbox_id != *desired_sandbox_id {
            continue;
        }
        let status = latest_sandbox_process_event_cursor(
            conversation,
            candidate.sandbox_id.clone(),
            candidate.process_id.clone(),
        )
        .await;
        let Ok(status) = status else {
            continue;
        };
        if status.status.is_running() {
            return Ok(Some((candidate.process_id, true, status.cursor)));
        }
    }

    Ok(None)
}

async fn latest_sandbox_process_event_cursor(
    conversation: &dyn ConversationHandle,
    sandbox_id: SandboxId,
    process_id: SandboxProcessId,
) -> Result<GetLatestSandboxProcessCursorResult> {
    latest_sandbox_process_event_cursor_from_fetch(|after| {
        let sandbox_id = sandbox_id.clone();
        let process_id = process_id.clone();
        async move {
            conversation
                .get_sandbox_process_events(SandboxProcessEventQuery {
                    sandbox_id,
                    process_id,
                    after,
                    limit: Some(1000),
                    follow: Some(false),
                })
                .await
        }
    })
    .await
}

pub(crate) async fn latest_sandbox_process_event_cursor_from_fetch<F, Fut>(
    mut fetch_page: F,
) -> Result<GetLatestSandboxProcessCursorResult>
where
    F: FnMut(Option<u64>) -> Fut,
    Fut: Future<Output = Result<GetSandboxProcessEventsResult>>,
{
    let mut after = None;
    loop {
        let previous_after = after;
        let page = fetch_page(after).await?;
        let event_count = page.events.len();
        after = page.cursor.or(after);
        if !page.status.is_running() || event_count < 1000 {
            return Ok(GetLatestSandboxProcessCursorResult {
                cursor: after,
                status: page.status,
            });
        }
        if after == previous_after {
            bail!("sandbox process event pagination did not advance");
        }
    }
}

pub(crate) struct GetLatestSandboxProcessCursorResult {
    pub(crate) cursor: Option<u64>,
    pub(crate) status: SandboxProcessStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeScriptInitPayload {
    pub mcp_servers: Vec<crate::NativeMcpServer>,
    pub tools: Vec<crate::ToolDefinition>,
    pub agent: exoharness::AgentRecord,
    pub conversation: ConversationHandleInfo,
    pub turn: TurnHandleInfo,
    pub agent_config: AgentConfig,
    pub conversation_config: ConversationConfig,
    pub request: SendRequest,
    pub streaming: bool,
    pub recovering: bool,
    pub braintrust_parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeRequest {
    AuthorizeTool {
        request: ToolRequest,
    },
    ExecuteTool {
        request: ToolRequest,
    },
    StartSandboxProcess {
        command: Vec<String>,
        env: HashMap<String, String>,
        reuse_key: Option<String>,
    },
    WriteSandboxProcessStdin {
        process_id: u64,
        data: String,
    },
    CloseSandboxProcessStdin {
        process_id: u64,
    },
    CloseSandboxProcess {
        process_id: u64,
    },
}

impl RuntimeRequest {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::AuthorizeTool { .. } => "authorize_tool",
            Self::ExecuteTool { .. } => "execute_tool",
            Self::StartSandboxProcess { .. } => "start_sandbox_process",
            Self::WriteSandboxProcessStdin { .. } => "write_sandbox_process_stdin",
            Self::CloseSandboxProcessStdin { .. } => "close_sandbox_process_stdin",
            Self::CloseSandboxProcess { .. } => "close_sandbox_process",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeResponsePayload {
    ToolResult {
        result: ToolResult,
    },
    SandboxProcessStarted {
        process_id: u64,
        sandbox_id: SandboxId,
        sandbox_process_id: SandboxProcessId,
        reused: bool,
    },
    Unit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxProcessStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RuntimeEvent {
    #[serde(rename = "sandbox_process_output")]
    Output {
        process_id: u64,
        stream: SandboxProcessStream,
        data: String,
    },
    #[serde(rename = "sandbox_process_exit")]
    Exit {
        process_id: u64,
        exit_code: Option<i32>,
    },
    #[serde(rename = "sandbox_process_error")]
    Error { process_id: u64, message: String },
}

fn spawn_process_events(
    host: &dyn RuntimeHost,
    emit: Arc<dyn Fn(RuntimeEvent) -> Result<()> + Send + Sync>,
    conversation: Arc<dyn ConversationHandle>,
    process_id: u64,
    sandbox_id: SandboxId,
    sandbox_process_id: SandboxProcessId,
    mut cursor: Option<u64>,
) -> AbortHandle {
    let (cancel, cancelled) = AbortHandle::new_pair();
    host.spawn(Box::pin(async move {
        if Abortable::new(
            async move {
                let mut decoders = [
                    encoding_rs::UTF_8.new_decoder_without_bom_handling(),
                    encoding_rs::UTF_8.new_decoder_without_bom_handling(),
                ];
                loop {
                    let result = conversation
                        .get_sandbox_process_events(SandboxProcessEventQuery {
                            sandbox_id: sandbox_id.clone(),
                            process_id: sandbox_process_id.clone(),
                            after: cursor,
                            limit: Some(100),
                            follow: Some(true),
                        })
                        .await;

                    let result = match result {
                        Ok(result) => result,
                        Err(error) => {
                            if let Err(error) = emit(RuntimeEvent::Error {
                                process_id,
                                message: error.to_string(),
                            }) {
                                tracing::debug!(%error, "TypeScript event consumer closed");
                            }
                            return;
                        }
                    };

                    let full_page = result.events.len() == 100;
                    for event in result.events {
                        cursor = Some(event.cursor());
                        if matches!(
                            event,
                            SandboxProcessEvent::Exit { .. }
                                | SandboxProcessEvent::Error { .. }
                                | SandboxProcessEvent::Cancelled { .. }
                        ) && flush_output(&emit, process_id, &mut decoders).is_err()
                        {
                            return;
                        }
                        let runtime_event = sandbox_process_event_to_runtime_event(
                            process_id,
                            event,
                            &mut decoders,
                        );
                        let terminal = matches!(
                            runtime_event,
                            RuntimeEvent::Exit { .. } | RuntimeEvent::Error { .. }
                        );
                        if emit(runtime_event).is_err() || terminal {
                            return;
                        }
                    }

                    if !full_page && !result.status.is_running() {
                        if flush_output(&emit, process_id, &mut decoders).is_err() {
                            return;
                        }
                        let runtime_event = match result.status {
                            exoharness::SandboxProcessStatus::Running => continue,
                            exoharness::SandboxProcessStatus::Exited { exit_code } => {
                                RuntimeEvent::Exit {
                                    process_id,
                                    exit_code: Some(exit_code),
                                }
                            }
                            exoharness::SandboxProcessStatus::Failed { message } => {
                                RuntimeEvent::Error {
                                    process_id,
                                    message,
                                }
                            }
                            exoharness::SandboxProcessStatus::Cancelled => RuntimeEvent::Exit {
                                process_id,
                                exit_code: None,
                            },
                        };
                        if emit(runtime_event).is_err() {
                            return;
                        }
                        return;
                    }
                }
            },
            cancelled,
        )
        .await
        .is_err()
        {
            tracing::debug!("TypeScript process event subscription cancelled");
        }
    }));
    cancel
}

fn sandbox_process_event_to_runtime_event(
    process_id: u64,
    event: SandboxProcessEvent,
    decoders: &mut [encoding_rs::Decoder; 2],
) -> RuntimeEvent {
    match event {
        SandboxProcessEvent::Stdout { data, .. } => RuntimeEvent::Output {
            process_id,
            stream: SandboxProcessStream::Stdout,
            data: decode_output(&mut decoders[0], &data, false),
        },
        SandboxProcessEvent::Stderr { data, .. } => RuntimeEvent::Output {
            process_id,
            stream: SandboxProcessStream::Stderr,
            data: decode_output(&mut decoders[1], &data, false),
        },
        SandboxProcessEvent::Exit { exit_code, .. } => RuntimeEvent::Exit {
            process_id,
            exit_code: Some(exit_code),
        },
        SandboxProcessEvent::Error { message, .. } => RuntimeEvent::Error {
            process_id,
            message,
        },
        SandboxProcessEvent::Cancelled { .. } => RuntimeEvent::Exit {
            process_id,
            exit_code: None,
        },
    }
}

fn decode_output(decoder: &mut encoding_rs::Decoder, bytes: &[u8], last: bool) -> String {
    let mut text = String::with_capacity(
        decoder
            .max_utf8_buffer_length(bytes.len())
            .expect("process output fits in memory"),
    );
    let (result, read, _replaced_invalid_utf8) = decoder.decode_to_string(bytes, &mut text, last);
    debug_assert_eq!(result, encoding_rs::CoderResult::InputEmpty);
    debug_assert_eq!(read, bytes.len());
    text
}

fn flush_output(
    emit: &Arc<dyn Fn(RuntimeEvent) -> Result<()> + Send + Sync>,
    process_id: u64,
    decoders: &mut [encoding_rs::Decoder; 2],
) -> Result<()> {
    for (decoder, stream) in decoders
        .iter_mut()
        .zip([SandboxProcessStream::Stdout, SandboxProcessStream::Stderr])
    {
        let data = decode_output(decoder, &[], true);
        if !data.is_empty() {
            emit(RuntimeEvent::Output {
                process_id,
                stream,
                data,
            })?;
        }
    }
    Ok(())
}
