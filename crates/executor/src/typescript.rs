use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as AnyhowContext, anyhow, bail};
use async_trait::async_trait;
use exoharness::{
    AgentHandle, BasicExoHarness, BasicExoHarnessConfig, BoxAsyncRead, BoxAsyncWrite,
    ConversationHandle, ExoHarness, Result, RunInSandboxRequest, ToolArguments, ToolRequest,
    ToolResult, TurnHandle,
    protocol::{
        ConversationHandleInfo, Request as ExoRequest, Response as ExoResponse, TurnHandleInfo,
    },
    server::ExoHarnessServer,
};
use futures::future::BoxFuture;
use futures::io::{AsyncReadExt as FuturesAsyncReadExt, AsyncWriteExt as FuturesAsyncWriteExt};
use lingua::UniversalStreamChunk;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter, Lines};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::conversation_sandbox::ensure_conversation_sandbox;
use crate::execution_tracing::TurnExecutionTrace;
use crate::harness_executor::{ExecutorHarnessRuntime, ExecutorStreamMode, HarnessExecutor};
use crate::harness_facade::{SharedHarness, SharedHarnessBacked};
use crate::harness_tool::{BasicToolRuntime, ExoclawToolRuntime};
use crate::shared::try_send_stream_event;
use crate::{
    AgentConfig, BraintrustRuntimeConfig, ConversationConfig, ExecutionStreamEvent, SendRequest,
    ToolRuntime,
};

pub struct TypeScriptExecutor<T> {
    root: Arc<dyn ExoHarness>,
    workspace_root: PathBuf,
    env: Arc<HashMap<String, String>>,
    tools: Arc<T>,
    runners: Arc<Mutex<HashMap<String, Arc<Mutex<TypeScriptRunnerProcess>>>>>,
}

impl<T> TypeScriptExecutor<T> {
    pub fn new(
        root: Arc<dyn ExoHarness>,
        workspace_root: PathBuf,
        env: HashMap<String, String>,
        tools: Arc<T>,
    ) -> Self {
        Self {
            root,
            workspace_root,
            env: Arc::new(env),
            tools,
            runners: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<T> Clone for TypeScriptExecutor<T> {
    fn clone(&self) -> Self {
        Self {
            root: Arc::clone(&self.root),
            workspace_root: self.workspace_root.clone(),
            env: Arc::clone(&self.env),
            tools: Arc::clone(&self.tools),
            runners: Arc::clone(&self.runners),
        }
    }
}

#[async_trait]
impl<T> HarnessExecutor for TypeScriptExecutor<T>
where
    T: ToolRuntime + 'static,
{
    type Prepared = SendRequest;

    async fn prepare_conversation(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
    ) -> Result<()> {
        self.tools
            .prepare_conversation(agent, conversation, agent_config, conversation_config)
            .await
    }

    fn prepare_request(&self, request: &SendRequest) -> Result<Self::Prepared> {
        Ok(request.clone())
    }

    async fn execute_turn(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        turn: Arc<dyn TurnHandle>,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        prepared: &Self::Prepared,
        stream_mode: ExecutorStreamMode<'_>,
        turn_trace: Option<&dyn TurnExecutionTrace>,
    ) -> Result<()> {
        let module_path = agent_config
            .typescript
            .as_ref()
            .map(|config| config.module_path.clone())
            .ok_or_else(|| anyhow!("typescript harness requires agent.typescript.module_path"))?;
        if !Path::new(&module_path).is_file() {
            bail!("typescript harness module does not exist: {module_path}");
        }

        let runner = self.runner(&module_path).await?;
        let result = {
            let mut runner = runner.lock().await;
            runner
                .execute_turn(
                    self,
                    TypeScriptTurn {
                        agent,
                        conversation,
                        turn,
                        agent_config,
                        conversation_config,
                        prepared,
                        stream_mode,
                        turn_trace,
                    },
                )
                .await
        };

        if result.is_err() {
            self.remove_runner(&module_path, &runner).await;
        }

        result
    }
}

impl<T> TypeScriptExecutor<T>
where
    T: ToolRuntime + 'static,
{
    async fn runner(&self, module_path: &str) -> Result<Arc<Mutex<TypeScriptRunnerProcess>>> {
        let mut runners = self.runners.lock().await;
        if let Some(runner) = runners.get(module_path) {
            return Ok(Arc::clone(runner));
        }

        let runner = Arc::new(Mutex::new(TypeScriptRunnerProcess::start(
            &self.workspace_root,
            self.env.as_ref(),
            module_path,
        )?));
        runners.insert(module_path.to_string(), Arc::clone(&runner));
        Ok(runner)
    }

    async fn remove_runner(&self, module_path: &str, runner: &Arc<Mutex<TypeScriptRunnerProcess>>) {
        let mut runners = self.runners.lock().await;
        if let Some(current) = runners.get(module_path)
            && Arc::ptr_eq(current, runner)
        {
            runners.remove(module_path);
        }
    }

    async fn execute_runtime_request(
        &self,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        _agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponsePayload> {
        match request {
            RuntimeRequest::ExecuteTool { request } => Ok(RuntimeResponsePayload::ToolResult {
                result: self
                    .tools
                    .execute(
                        agent,
                        conversation,
                        _agent_config,
                        conversation_config,
                        &request,
                    )
                    .await?,
            }),
            RuntimeRequest::StartSandboxProcess { .. }
            | RuntimeRequest::WriteSandboxProcessStdin { .. }
            | RuntimeRequest::CloseSandboxProcessStdin { .. }
            | RuntimeRequest::CloseSandboxProcess { .. } => {
                bail!("sandbox process requests are handled by the TypeScript runner")
            }
        }
    }
}

const RUNNER_EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

struct TypeScriptRunnerProcess {
    child: Child,
    host_tx: mpsc::UnboundedSender<HostToGuestMessage>,
    lines: Lines<BufReader<ChildStdout>>,
    stderr_task: Option<JoinHandle<std::io::Result<String>>>,
    _writer_task: JoinHandle<anyhow::Result<()>>,
    next_sandbox_process_id: u64,
    sandbox_processes: HashMap<u64, RunningSandboxProcess>,
}

struct RunningSandboxProcess {
    stdin: Option<BoxAsyncWrite>,
    stdout_task: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    wait_task: JoinHandle<()>,
}

struct TypeScriptTurn<'a> {
    agent: &'a dyn AgentHandle,
    conversation: &'a dyn ConversationHandle,
    turn: Arc<dyn TurnHandle>,
    agent_config: &'a AgentConfig,
    conversation_config: &'a ConversationConfig,
    prepared: &'a SendRequest,
    stream_mode: ExecutorStreamMode<'a>,
    turn_trace: Option<&'a dyn TurnExecutionTrace>,
}

impl TypeScriptRunnerProcess {
    fn start(
        workspace_root: &Path,
        env: &HashMap<String, String>,
        module_path: &str,
    ) -> Result<Self> {
        let runner_path = workspace_root
            .join("typescript")
            .join("harness")
            .join("runner.ts");
        if !runner_path.is_file() {
            bail!(
                "typescript harness runner does not exist: {}",
                runner_path.display()
            );
        }

        let mut child = Command::new("node")
            .arg("--import")
            .arg("tsx")
            .arg(&runner_path)
            .arg(module_path)
            .current_dir(workspace_root)
            .envs(env.iter())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!("failed to start TypeScript harness runner for {module_path}")
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("typescript harness runner did not expose stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("typescript harness runner did not expose stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("typescript harness runner did not expose stderr"))?;

        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        });

        let (host_tx, mut host_rx) = mpsc::unbounded_channel::<HostToGuestMessage>();
        let writer_task = tokio::spawn(async move {
            let mut stdin = BufWriter::new(stdin);
            while let Some(message) = host_rx.recv().await {
                write_protocol_message(&mut stdin, &message).await?;
            }
            Ok(())
        });

        Ok(Self {
            child,
            host_tx,
            lines: BufReader::new(stdout).lines(),
            stderr_task: Some(stderr_task),
            _writer_task: writer_task,
            next_sandbox_process_id: 1,
            sandbox_processes: HashMap::new(),
        })
    }

    async fn execute_turn<T>(
        &mut self,
        executor: &TypeScriptExecutor<T>,
        turn: TypeScriptTurn<'_>,
    ) -> Result<()>
    where
        T: ToolRuntime + 'static,
    {
        let TypeScriptTurn {
            agent,
            conversation,
            turn,
            agent_config,
            conversation_config,
            prepared,
            stream_mode,
            turn_trace,
        } = turn;
        let exoharness_server = ExoHarnessServer::new(Arc::clone(&executor.root));
        let conversation_info = ConversationHandleInfo {
            agent_id: agent.record().id,
            record: conversation.record().clone(),
        };
        let turn_info = exoharness_server.register_turn(
            agent.record().id,
            conversation.record().clone(),
            Arc::clone(&turn),
        );
        send_host_message(
            &self.host_tx,
            HostToGuestMessage::Init {
                payload: Box::new(TypeScriptInitPayload {
                    agent: agent.record().clone(),
                    conversation: conversation_info,
                    turn: turn_info,
                    agent_config: agent_config.clone(),
                    conversation_config: conversation_config.clone(),
                    request: prepared.clone(),
                    streaming: matches!(stream_mode, ExecutorStreamMode::Enabled(_)),
                    braintrust_parent: turn_trace.and_then(TurnExecutionTrace::export_parent),
                }),
            },
        )?;

        loop {
            tokio::select! {
                biased;

                line = self.lines.next_line() => {
                    let Some(line) = line? else {
                        let status = self.child.wait().await?;
                        return Err(self.exited_error(status).await);
                    };
                    let message: GuestToHostMessage = serde_json::from_str(&line).map_err(|error| {
                        anyhow!("invalid TypeScript harness protocol message: {line}\ncaused by: {error}")
                    })?;
                    match message {
                        GuestToHostMessage::RuntimeRequest { id, request } => {
                            let request_kind = request.kind();
                            let response = self
                                .execute_runtime_request(
                                    executor,
                                    agent,
                                    conversation,
                                    agent_config,
                                    conversation_config,
                                    request,
                                )
                                .await;
                            let response = match response {
                                Ok(payload) => HostToGuestMessage::RuntimeResponse {
                                    id,
                                    ok: true,
                                    payload: Some(payload),
                                    error: None,
                                },
                                Err(error) => HostToGuestMessage::RuntimeResponse {
                                    id,
                                    ok: false,
                                    payload: None,
                                    error: Some(format_error_chain(
                                        &error,
                                        format_args!("typescript runtime request `{request_kind}` failed"),
                                    )),
                                },
                            };
                            send_host_message(&self.host_tx, response)?;
                        }
                        GuestToHostMessage::ExoRequest { id, request } => {
                            let response = match exoharness_server.handle_request(request).await {
                                Ok(response) => HostToGuestMessage::ExoResponse {
                                    id,
                                    ok: true,
                                    response: Some(response),
                                    error: None,
                                },
                                Err(error) => HostToGuestMessage::ExoResponse {
                                    id,
                                    ok: false,
                                    response: None,
                                    error: Some(format_error_chain(
                                        &error,
                                        format_args!("typescript exoharness request failed"),
                                    )),
                                },
                            };
                            send_host_message(&self.host_tx, response)?;
                        }
                        GuestToHostMessage::StreamEvent { event } => {
                            if let ExecutorStreamMode::Enabled(event_tx) = stream_mode {
                                try_send_stream_event(event_tx, to_execution_stream_event(event));
                            }
                        }
                        GuestToHostMessage::Done => return Ok(()),
                        GuestToHostMessage::Error { message, stack } => {
                            let stack_suffix = stack
                                .as_deref()
                                .map(|stack| format!("\n{stack}"))
                                .unwrap_or_default();
                            bail!("typescript harness failed: {message}{stack_suffix}");
                        }
                    }
                }
                () = tokio::time::sleep(RUNNER_EXIT_POLL_INTERVAL) => {
                    if let Some(status) = self.child.try_wait()? {
                        return Err(self.exited_error(status).await);
                    }
                }
            }
        }
    }

    async fn execute_runtime_request<T>(
        &mut self,
        executor: &TypeScriptExecutor<T>,
        agent: &dyn AgentHandle,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        request: RuntimeRequest,
    ) -> Result<RuntimeResponsePayload>
    where
        T: ToolRuntime + 'static,
    {
        match request {
            RuntimeRequest::ExecuteTool { request } => {
                executor
                    .execute_runtime_request(
                        agent,
                        conversation,
                        agent_config,
                        conversation_config,
                        RuntimeRequest::ExecuteTool { request },
                    )
                    .await
            }
            RuntimeRequest::StartSandboxProcess { command, env } => {
                self.start_sandbox_process(
                    conversation,
                    agent_config,
                    conversation_config,
                    command,
                    env,
                )
                .await
            }
            RuntimeRequest::WriteSandboxProcessStdin { process_id, data } => {
                let process = self
                    .sandbox_processes
                    .get_mut(&process_id)
                    .ok_or_else(|| anyhow!("sandbox process is not active: {process_id}"))?;
                let stdin = process
                    .stdin
                    .as_mut()
                    .ok_or_else(|| anyhow!("sandbox process stdin is closed: {process_id}"))?;
                FuturesAsyncWriteExt::write_all(stdin, data.as_bytes()).await?;
                FuturesAsyncWriteExt::flush(stdin).await?;
                Ok(RuntimeResponsePayload::Unit)
            }
            RuntimeRequest::CloseSandboxProcessStdin { process_id } => {
                let process = self
                    .sandbox_processes
                    .get_mut(&process_id)
                    .ok_or_else(|| anyhow!("sandbox process is not active: {process_id}"))?;
                process.stdin.take();
                Ok(RuntimeResponsePayload::Unit)
            }
            RuntimeRequest::CloseSandboxProcess { process_id } => {
                if let Some(process) = self.sandbox_processes.remove(&process_id) {
                    process.stdout_task.abort();
                    process.stderr_task.abort();
                    process.wait_task.abort();
                    send_host_message(
                        &self.host_tx,
                        HostToGuestMessage::RuntimeEvent {
                            event: RuntimeEvent::Exit {
                                process_id,
                                exit_code: None,
                            },
                        },
                    )?;
                }
                Ok(RuntimeResponsePayload::Unit)
            }
        }
    }

    async fn start_sandbox_process(
        &mut self,
        conversation: &dyn ConversationHandle,
        agent_config: &AgentConfig,
        conversation_config: &ConversationConfig,
        command: Vec<String>,
        env: HashMap<String, String>,
    ) -> Result<RuntimeResponsePayload> {
        let sandbox_id =
            ensure_conversation_sandbox(conversation, agent_config, conversation_config).await?;
        let process = conversation
            .run_in_sandbox(RunInSandboxRequest {
                id: sandbox_id,
                command,
                env,
            })
            .await?;
        let parts = process.into_parts();
        let process_id = self.next_sandbox_process_id;
        self.next_sandbox_process_id += 1;

        let stdout_task = spawn_sandbox_output_task(
            self.host_tx.clone(),
            process_id,
            SandboxProcessStream::Stdout,
            parts.stdout,
        );
        let stderr_task = spawn_sandbox_output_task(
            self.host_tx.clone(),
            process_id,
            SandboxProcessStream::Stderr,
            parts.stderr,
        );
        let wait_task = spawn_sandbox_wait_task(self.host_tx.clone(), process_id, parts.wait);

        self.sandbox_processes.insert(
            process_id,
            RunningSandboxProcess {
                stdin: Some(parts.stdin),
                stdout_task,
                stderr_task,
                wait_task,
            },
        );

        Ok(RuntimeResponsePayload::SandboxProcessStarted { process_id })
    }

    async fn exited_error(&mut self, status: ExitStatus) -> anyhow::Error {
        let stderr_output = self.take_stderr_output().await;
        let stderr_suffix = if stderr_output.trim().is_empty() {
            String::new()
        } else {
            format!("\nstderr:\n{}", stderr_output.trim())
        };
        anyhow!(
            "typescript harness runner exited with status {}{}",
            status,
            stderr_suffix
        )
    }

    async fn take_stderr_output(&mut self) -> String {
        let Some(stderr_task) = self.stderr_task.take() else {
            return String::new();
        };
        match stderr_task.await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => format!("failed to read TypeScript harness stderr: {error}"),
            Err(error) => format!("TypeScript harness stderr task panicked: {error}"),
        }
    }
}

pub struct TypeScriptHarness<T> {
    inner: SharedHarness<ExecutorHarnessRuntime<TypeScriptExecutor<T>>>,
}

impl<T> TypeScriptHarness<T> {
    pub fn new(exoharness: Arc<dyn ExoHarness>, workspace_root: PathBuf, tools: Arc<T>) -> Self
    where
        T: ToolRuntime + 'static,
    {
        let runtime = ExecutorHarnessRuntime::new(
            TypeScriptExecutor::new(
                Arc::clone(&exoharness),
                workspace_root,
                HashMap::new(),
                tools,
            ),
            None,
        );
        Self {
            inner: SharedHarness::new(exoharness, runtime),
        }
    }
}

impl TypeScriptHarness<BasicToolRuntime> {
    pub async fn from_config(
        exo_config: BasicExoHarnessConfig,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let workspace_root = std::env::current_dir()
            .context("failed to resolve current directory for TypeScript harness")?;
        let exoharness: Arc<dyn ExoHarness> = Arc::new(BasicExoHarness::new(exo_config).await?);
        let tools = Arc::new(BasicToolRuntime);
        let runtime = ExecutorHarnessRuntime::new(
            TypeScriptExecutor::new(Arc::clone(&exoharness), workspace_root, env, tools),
            runtime_config,
        );
        Ok(Self {
            inner: SharedHarness::new(exoharness, runtime),
        })
    }
}

impl TypeScriptHarness<ExoclawToolRuntime> {
    pub async fn exoclaw_from_root(
        root: impl AsRef<Path>,
        exo_config: BasicExoHarnessConfig,
        runtime_config: Option<BraintrustRuntimeConfig>,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let workspace_root = std::env::current_dir()
            .context("failed to resolve current directory for Exoclaw harness")?;
        let root = root.as_ref();
        let exoharness: Arc<dyn ExoHarness> = Arc::new(BasicExoHarness::new(exo_config).await?);
        let tools = Arc::new(ExoclawToolRuntime::with_roots(
            root.join("scheduled-tasks"),
            root.join("adapters"),
        ));
        let runtime = ExecutorHarnessRuntime::new(
            TypeScriptExecutor::new(Arc::clone(&exoharness), workspace_root, env, tools),
            runtime_config,
        );
        Ok(Self {
            inner: SharedHarness::new(exoharness, runtime),
        })
    }
}

impl<T> SharedHarnessBacked for TypeScriptHarness<T>
where
    T: ToolRuntime + 'static,
{
    type Runtime = ExecutorHarnessRuntime<TypeScriptExecutor<T>>;

    fn shared_harness(&self) -> &SharedHarness<Self::Runtime> {
        &self.inner
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum HostToGuestMessage {
    Init {
        payload: Box<TypeScriptInitPayload>,
    },
    Shutdown,
    RuntimeResponse {
        id: u64,
        ok: bool,
        payload: Option<RuntimeResponsePayload>,
        error: Option<String>,
    },
    ExoResponse {
        id: u64,
        ok: bool,
        response: Option<ExoResponse>,
        error: Option<String>,
    },
    RuntimeEvent {
        event: RuntimeEvent,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GuestToHostMessage {
    RuntimeRequest {
        id: u64,
        request: RuntimeRequest,
    },
    ExoRequest {
        id: u64,
        request: ExoRequest,
    },
    StreamEvent {
        event: TypeScriptStreamEvent,
    },
    Done,
    Error {
        message: String,
        stack: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypeScriptInitPayload {
    agent: exoharness::AgentRecord,
    conversation: ConversationHandleInfo,
    turn: TurnHandleInfo,
    agent_config: AgentConfig,
    conversation_config: ConversationConfig,
    request: SendRequest,
    streaming: bool,
    braintrust_parent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RuntimeRequest {
    ExecuteTool {
        request: ToolRequest,
    },
    StartSandboxProcess {
        command: Vec<String>,
        env: HashMap<String, String>,
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
    fn kind(&self) -> &'static str {
        match self {
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
enum RuntimeResponsePayload {
    ToolResult { result: ToolResult },
    SandboxProcessStarted { process_id: u64 },
    Unit,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SandboxProcessStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum RuntimeEvent {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TypeScriptStreamEvent {
    FirstChunk {
        ttft_ms: u64,
    },
    TextDelta {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        tool_name: String,
        arguments: ToolArguments,
    },
    ToolResult {
        tool_call_id: String,
        result: ToolResult,
    },
}

fn to_execution_stream_event(event: TypeScriptStreamEvent) -> ExecutionStreamEvent {
    match event {
        TypeScriptStreamEvent::FirstChunk { ttft_ms } => ExecutionStreamEvent::FirstChunk {
            ttft: Duration::from_millis(ttft_ms),
        },
        TypeScriptStreamEvent::TextDelta { text } => {
            ExecutionStreamEvent::Chunk(UniversalStreamChunk::text_delta(0, &text))
        }
        TypeScriptStreamEvent::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        } => ExecutionStreamEvent::ToolCall {
            tool_call_id,
            tool_name,
            arguments,
        },
        TypeScriptStreamEvent::ToolResult {
            tool_call_id,
            result,
        } => ExecutionStreamEvent::ToolResult {
            tool_call_id,
            result,
        },
    }
}

fn spawn_sandbox_output_task(
    sender: mpsc::UnboundedSender<HostToGuestMessage>,
    process_id: u64,
    stream: SandboxProcessStream,
    mut reader: BoxAsyncRead,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = vec![0; 8192];
        loop {
            match FuturesAsyncReadExt::read(&mut reader, &mut buffer).await {
                Ok(0) => return,
                Ok(length) => {
                    let data = String::from_utf8_lossy(&buffer[..length]).into_owned();
                    if send_host_message(
                        &sender,
                        HostToGuestMessage::RuntimeEvent {
                            event: RuntimeEvent::Output {
                                process_id,
                                stream,
                                data,
                            },
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                }
                Err(error) => {
                    if send_host_message(
                        &sender,
                        HostToGuestMessage::RuntimeEvent {
                            event: RuntimeEvent::Error {
                                process_id,
                                message: error.to_string(),
                            },
                        },
                    )
                    .is_err()
                    {
                        return;
                    }
                    return;
                }
            }
        }
    })
}

fn spawn_sandbox_wait_task(
    sender: mpsc::UnboundedSender<HostToGuestMessage>,
    process_id: u64,
    wait: BoxFuture<'static, Result<i32>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let event = match wait.await {
            Ok(exit_code) => RuntimeEvent::Exit {
                process_id,
                exit_code: Some(exit_code),
            },
            Err(error) => RuntimeEvent::Error {
                process_id,
                message: error.to_string(),
            },
        };
        if send_host_message(&sender, HostToGuestMessage::RuntimeEvent { event }).is_err() {
            // The runner has gone away, so there is no receiver for this event.
        }
    })
}

fn send_host_message(
    sender: &mpsc::UnboundedSender<HostToGuestMessage>,
    message: HostToGuestMessage,
) -> Result<()> {
    sender
        .send(message)
        .map_err(|_| anyhow!("typescript harness runner stdin closed"))
}

async fn write_protocol_message(
    writer: &mut BufWriter<tokio::process::ChildStdin>,
    message: &HostToGuestMessage,
) -> Result<()> {
    let encoded = serde_json::to_vec(message)?;
    writer.write_all(&encoded).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

fn format_error_chain(error: &anyhow::Error, context: std::fmt::Arguments<'_>) -> String {
    let mut message = context.to_string();
    for (index, cause) in error.chain().enumerate() {
        if index == 0 {
            message.push_str(": ");
        } else {
            message.push_str("\ncaused by: ");
        }
        message.push_str(&cause.to_string());
    }
    message
}
