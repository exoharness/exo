use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine;
use exoharness::{
    AgentHandle, ConversationHandle, Result, RunInSandboxRequest, SandboxProcess, ToolRequest,
    ToolResult, Uuid7,
};
use futures::{StreamExt, io::AsyncReadExt};
use serde::Deserialize;
use serde_json::Value;
use tokio::fs;

use super::runtime::send_adapter_message_with_handles;
use super::store::AdapterStore;
use super::types::{
    AdapterAttachment, AdapterConfig, AdapterEventType, AdapterSource, NewAdapter,
    WorkerSecretEnvVar,
};
use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::ensure_conversation_sandbox;
use crate::{AgentConfig, ConversationConfig, SandboxScope, effective_sandbox_scope};

const DEFAULT_EVENT_LIMIT: usize = 50;
const MAX_EVENT_LIMIT: usize = 200;
const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
const MAX_ATTACHMENT_BASE64_BYTES: usize = MAX_ATTACHMENT_BYTES.div_ceil(3) * 4 + 4;
const ATTACHMENT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTACHMENT_STDERR_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAdapterArguments {
    name: String,
    source: AdapterSource,
    config: AdapterCreationConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationScopedArguments {
    include_disabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterIdArguments {
    adapter_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAdapterEventsArguments {
    adapter_id: String,
    event_type: Option<AdapterEventType>,
    since_ms: Option<u64>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendAdapterMessageArguments {
    adapter_id: String,
    text: String,
    target: Option<String>,
    #[serde(default)]
    attachments: Option<Vec<AdapterAttachment>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AdapterCreationConfig {
    Irc(IrcAdapterCreationConfig),
    Whatsapp(WhatsappAdapterCreationConfig),
    Signal(SignalAdapterCreationConfig),
    Discord(DiscordAdapterCreationConfig),
    AgentCli(AgentCliAdapterCreationConfig),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IrcAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: IrcAdapterType,
    server: String,
    port: u16,
    tls: bool,
    nick: String,
    username: String,
    realname: String,
    channel: String,
    password_secret_id: Option<String>,
    trigger: IrcTrigger,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WhatsappAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: WhatsappAdapterType,
    auth_dir: Option<String>,
    link_method: Option<WhatsappLinkMethod>,
    phone_number: Option<String>,
    trigger: ChatTrigger,
    allowed_chats: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SignalAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: SignalAdapterType,
    account: Option<String>,
    device_name: Option<String>,
    config_dir: Option<String>,
    trigger: ChatTrigger,
    allowed_contacts: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiscordAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: DiscordAdapterType,
    bot_token_secret_id: String,
    default_channel_id: Option<String>,
    trigger: DiscordTrigger,
    allowed_channels: Option<Vec<String>>,
    allow_bots: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentCliAdapterCreationConfig {
    #[serde(rename = "type")]
    _adapter_type: AgentCliAdapterType,
    socket_path: Option<String>,
    mount_root: String,
    mount_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum IrcAdapterType {
    Irc,
}

#[derive(Debug, Deserialize)]
enum AgentCliAdapterType {
    #[serde(rename = "agent-cli")]
    AgentCli,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WhatsappAdapterType {
    Whatsapp,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum WhatsappLinkMethod {
    Qr,
    PairingCode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SignalAdapterType {
    Signal,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DiscordAdapterType {
    Discord,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IrcTrigger {
    Mention,
    AllMessages,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChatTrigger {
    AllMessages,
    ContactsOnly,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DiscordTrigger {
    AllMessages,
    MentionsOnly,
}

fn default_irc_nick() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() % 10_000)
        .unwrap_or(0);
    format!("exoclaw{:04}", suffix)
}

impl AdapterCreationConfig {
    fn adapter_type(&self) -> &'static str {
        match self {
            Self::Irc(_) => "irc",
            Self::Whatsapp(_) => "whatsapp",
            Self::Signal(_) => "signal",
            Self::Discord(_) => "discord",
            Self::AgentCli(_) => "agent-cli",
        }
    }

    fn into_adapter_config(self, source: AdapterSource) -> Result<AdapterConfig> {
        match self {
            Self::Irc(config) => {
                require_source(source, AdapterSource::BuiltIn, "irc")?;
                Ok(AdapterConfig {
                    adapter_type: "irc".to_string(),
                    worker_command: bundled_worker_command(
                        "examples/exoclaw/adapters/irc/worker.ts",
                    ),
                    initialization: serde_json::json!({
                        "server": config.server,
                        "port": config.port,
                        "tls": config.tls,
                        "nick": if config.nick.trim().is_empty() { default_irc_nick() } else { config.nick },
                        "username": if config.username.trim().is_empty() { "exoclaw".to_string() } else { config.username },
                        "realname": config.realname,
                        "channel": config.channel,
                        "trigger": config.trigger.as_str(),
                    }),
                    state_dir: None,
                    secret_env: config
                        .password_secret_id
                        .map(|secret_id| {
                            vec![WorkerSecretEnvVar {
                                env: "EXO_IRC_PASSWORD".to_string(),
                                secret_id,
                            }]
                        })
                        .unwrap_or_default(),
                })
            }
            Self::Whatsapp(config) => {
                require_source(source, AdapterSource::Library, "whatsapp")?;
                if matches!(config.link_method, Some(WhatsappLinkMethod::PairingCode))
                    && config
                        .phone_number
                        .as_deref()
                        .is_none_or(|phone_number| phone_number.trim().is_empty())
                {
                    bail!("whatsapp pairing-code linkMethod requires phoneNumber");
                }
                Ok(AdapterConfig {
                    adapter_type: "whatsapp".to_string(),
                    worker_command: bundled_worker_command(
                        "examples/exoclaw/adapters/whatsapp/worker.ts",
                    ),
                    initialization: serde_json::json!({
                        "authDir": config.auth_dir,
                        "linkMethod": config.link_method.map(|method| method.as_str()),
                        "phoneNumber": config.phone_number,
                        "trigger": config.trigger.as_str(),
                        "allowedChats": config.allowed_chats,
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
            Self::Signal(config) => {
                require_source(source, AdapterSource::Library, "signal")?;
                Ok(AdapterConfig {
                    adapter_type: "signal".to_string(),
                    worker_command: bundled_worker_command(
                        "examples/exoclaw/adapters/signal/worker.ts",
                    ),
                    initialization: serde_json::json!({
                        "account": config.account,
                        "deviceName": config.device_name,
                        "configDir": config.config_dir,
                        "trigger": config.trigger.as_str(),
                        "allowedContacts": config.allowed_contacts,
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
            Self::Discord(config) => {
                require_source(source, AdapterSource::Library, "discord")?;
                Ok(AdapterConfig {
                    adapter_type: "discord".to_string(),
                    worker_command: bundled_worker_command(
                        "examples/exoclaw/adapters/discord/worker.ts",
                    ),
                    initialization: serde_json::json!({
                        "tokenEnv": "EXO_DISCORD_BOT_TOKEN",
                        "defaultChannelId": config.default_channel_id,
                        "trigger": config.trigger.as_str(),
                        "allowedChannels": config.allowed_channels,
                        "allowBots": config.allow_bots,
                    }),
                    state_dir: None,
                    secret_env: vec![WorkerSecretEnvVar {
                        env: "EXO_DISCORD_BOT_TOKEN".to_string(),
                        secret_id: config.bot_token_secret_id,
                    }],
                })
            }
            Self::AgentCli(config) => {
                require_source(source, AdapterSource::BuiltIn, "agent-cli")?;
                if !config.mount_root.starts_with('/') {
                    bail!("agent-cli mountRoot must be an absolute host path");
                }
                let mount_path = config
                    .mount_path
                    .unwrap_or_else(|| "/agent-cli".to_string());
                if !mount_path.starts_with('/') {
                    bail!("agent-cli mountPath must be an absolute sandbox path");
                }
                Ok(AdapterConfig {
                    adapter_type: "agent-cli".to_string(),
                    worker_command: bundled_worker_command(
                        "examples/exoclaw/adapters/agent-cli/worker.ts",
                    ),
                    initialization: serde_json::json!({
                        "socketPath": config.socket_path,
                        "mountRoot": config.mount_root,
                        "mountPath": mount_path,
                    }),
                    state_dir: None,
                    secret_env: Vec::new(),
                })
            }
        }
    }
}

impl IrcTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::AllMessages => "all_messages",
        }
    }
}

impl ChatTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::AllMessages => "all_messages",
            Self::ContactsOnly => "contacts_only",
        }
    }
}

impl WhatsappLinkMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Qr => "qr",
            Self::PairingCode => "pairing-code",
        }
    }
}

impl DiscordTrigger {
    fn as_str(&self) -> &'static str {
        match self {
            Self::AllMessages => "all_messages",
            Self::MentionsOnly => "mentions_only",
        }
    }
}

fn require_source(
    actual: AdapterSource,
    expected: AdapterSource,
    adapter_type: &str,
) -> Result<()> {
    if actual != expected {
        bail!("{adapter_type} adapters must use source {expected:?}");
    }
    Ok(())
}

fn bundled_worker_command(relative_path: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative_path);
    vec![
        "pnpm".to_string(),
        "tsx".to_string(),
        path.to_string_lossy().into_owned(),
    ]
}

pub async fn execute_create_adapter_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<CreateAdapterArguments>(Value::Object(request.arguments.clone()))?;
    let adapter_type = args.config.adapter_type();
    let config = args.config.into_adapter_config(args.source)?;
    let adapter = store
        .create_adapter(NewAdapter {
            agent_id: agent.record().id.to_string(),
            conversation_id: conversation.record().id.to_string(),
            name: args.name,
            source: args.source,
            config,
        })
        .await?;
    if adapter.config.adapter_type != adapter_type {
        bail!("created adapter type mismatch");
    }
    Ok(serde_json::json!({
        "ok": true,
        "adapter": adapter,
    }))
}

pub async fn execute_list_adapters_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ConversationScopedArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let adapters = store
        .list_adapters_for_conversation(
            &agent.record().id.to_string(),
            &conversation.record().id.to_string(),
            args.include_disabled.unwrap_or(false),
        )
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapters": adapters,
    }))
}

pub async fn execute_list_adapter_events_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ListAdapterEventsArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    let limit = args
        .limit
        .unwrap_or(DEFAULT_EVENT_LIMIT)
        .clamp(1, MAX_EVENT_LIMIT);
    let events = store
        .list_events(&args.adapter_id, args.event_type, args.since_ms, limit)
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "adapterName": adapter.name,
        "events": events,
    }))
}

pub async fn execute_disable_adapter_tool(
    conversation: &dyn ConversationHandle,
    agent: &dyn AgentHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<AdapterIdArguments>(Value::Object(request.arguments.clone()))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    store.disable_adapter(&args.adapter_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "disabled": true,
    }))
}

pub async fn execute_delete_adapter_tool(
    conversation: &dyn ConversationHandle,
    agent: &dyn AgentHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<AdapterIdArguments>(Value::Object(request.arguments.clone()))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    store.delete_adapter(&args.adapter_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "deleted": true,
        "eventsDeleted": true,
    }))
}

pub async fn execute_send_adapter_message_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<SendAdapterMessageArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != agent.record().id.to_string()
        || adapter.conversation_id != conversation.record().id.to_string()
    {
        return Ok(not_found());
    }
    let attachments = args.attachments.unwrap_or_default();
    if !attachments.is_empty()
        && adapter.config.adapter_type != "whatsapp"
        && adapter.config.adapter_type != "signal"
        && adapter.config.adapter_type != "discord"
    {
        bail!(
            "adapter {} does not support rich attachments",
            adapter.config.adapter_type
        );
    }
    let attachments = resolve_sandbox_attachments(
        agent,
        conversation,
        agent_config,
        config,
        store,
        &adapter,
        attachments,
    )
    .await?;
    send_adapter_message_with_handles(
        agent,
        conversation,
        store,
        &adapter,
        &args.text,
        args.target.as_deref(),
        attachments,
    )
    .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "sent": true,
    }))
}

async fn resolve_sandbox_attachments(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    store: &AdapterStore,
    adapter: &super::types::AdapterRecord,
    attachments: Vec<AdapterAttachment>,
) -> Result<Vec<AdapterAttachment>> {
    let mut resolved = Vec::with_capacity(attachments.len());
    for mut attachment in attachments {
        attachment.validate()?;
        if attachment.path.is_some() {
            bail!("attachment path is host-internal; use sandboxPath, url, or data");
        }
        if let Some(url) = attachment.url.clone() {
            let bytes = download_attachment(&url).await?;
            let path = stage_attachment(store, &adapter.id, &url, &attachment, bytes).await?;
            attachment.path = Some(path.to_string_lossy().into_owned());
            attachment.url = None;
            resolved.push(attachment);
            continue;
        }
        if let Some(data) = attachment.data.clone() {
            let bytes = decode_attachment_data(&data)?;
            if bytes.len() > MAX_ATTACHMENT_BYTES {
                bail!(
                    "attachment data is too large: {} bytes exceeds {} bytes",
                    bytes.len(),
                    MAX_ATTACHMENT_BYTES
                );
            }
            let path =
                stage_attachment(store, &adapter.id, "attachment", &attachment, bytes).await?;
            attachment.path = Some(path.to_string_lossy().into_owned());
            attachment.data = None;
            resolved.push(attachment);
            continue;
        }
        let Some(sandbox_path) = attachment.sandbox_path.clone() else {
            resolved.push(attachment);
            continue;
        };
        let bytes =
            read_sandbox_file(agent, conversation, agent_config, config, &sandbox_path).await?;
        let path = stage_attachment(store, &adapter.id, &sandbox_path, &attachment, bytes).await?;
        attachment.path = Some(path.to_string_lossy().into_owned());
        attachment.sandbox_path = None;
        resolved.push(attachment);
    }
    Ok(resolved)
}

async fn read_sandbox_file(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    sandbox_path: &str,
) -> Result<Vec<u8>> {
    match effective_sandbox_scope(agent_config, config) {
        SandboxScope::Agent => {
            let sandbox = ensure_agent_sandbox(agent, conversation, agent_config, config).await?;
            read_sandbox_file_bytes(
                sandbox.conversation.as_ref(),
                sandbox.sandbox_id,
                sandbox_path,
            )
            .await
        }
        SandboxScope::Conversation => {
            let sandbox_id =
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            read_sandbox_file_bytes(conversation, sandbox_id, sandbox_path).await
        }
    }
}

async fn download_attachment(url: &str) -> Result<Vec<u8>> {
    let client = reqwest::Client::builder()
        .timeout(ATTACHMENT_DOWNLOAD_TIMEOUT)
        .build()
        .context("failed to create adapter attachment download client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download adapter attachment {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download adapter attachment {url}"))?;
    if let Some(content_length) = response.content_length()
        && content_length > MAX_ATTACHMENT_BYTES as u64
    {
        bail!(
            "attachment is too large: {} bytes exceeds {} bytes",
            content_length,
            MAX_ATTACHMENT_BYTES
        );
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed to read adapter attachment {url}"))?;
        if bytes.len() + chunk.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "attachment is too large: more than {} bytes",
                MAX_ATTACHMENT_BYTES
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn decode_attachment_data(data: &str) -> Result<Vec<u8>> {
    let payload = data
        .strip_prefix("data:")
        .and_then(|_| data.split_once(',').map(|(_, payload)| payload))
        .unwrap_or(data);
    if payload.len() > MAX_ATTACHMENT_BASE64_BYTES {
        bail!(
            "attachment data is too large: base64 payload exceeds {} bytes",
            MAX_ATTACHMENT_BASE64_BYTES
        );
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .context("failed to decode attachment data")
}

async fn read_sandbox_file_bytes(
    conversation: &dyn ConversationHandle,
    sandbox_id: String,
    sandbox_path: &str,
) -> Result<Vec<u8>> {
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "cat -- \"$1\"".to_string(),
                "exo-read-attachment".to_string(),
                sandbox_path.to_string(),
            ],
            env: Default::default(),
        })
        .await?;
    let output = read_process_limited(process, MAX_ATTACHMENT_BYTES).await?;
    if output.exit_code != 0 {
        bail!(
            "failed to read sandbox attachment {}: {}",
            sandbox_path,
            output.stderr.trim()
        );
    }
    if output.truncated {
        bail!(
            "sandbox attachment is too large: exceeds {} bytes",
            MAX_ATTACHMENT_BYTES
        );
    }
    Ok(output.stdout)
}

struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: String,
    exit_code: i32,
    truncated: bool,
}

async fn read_process_limited(
    process: Box<dyn SandboxProcess>,
    max_stdout_bytes: usize,
) -> Result<ProcessOutput> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        read_limited(&mut stdout, max_stdout_bytes),
        read_limited(&mut stderr, MAX_ATTACHMENT_STDERR_BYTES),
        parts.wait,
    );
    let (stdout_bytes, stdout_truncated) = stdout_result?;
    let (stderr_bytes, stderr_truncated) = stderr_result?;
    let exit_code = wait_result?;

    Ok(ProcessOutput {
        stdout: stdout_bytes,
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
        truncated: stdout_truncated || stderr_truncated,
    })
}

async fn read_limited<R>(reader: &mut R, max_bytes: usize) -> Result<(Vec<u8>, bool)>
where
    R: futures::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = max_bytes.saturating_sub(output.len());
        if read > remaining {
            output.extend_from_slice(&buffer[..remaining]);
            truncated = true;
            continue;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok((output, truncated))
}

async fn stage_attachment(
    store: &AdapterStore,
    adapter_id: &str,
    sandbox_path: &str,
    attachment: &AdapterAttachment,
    bytes: Vec<u8>,
) -> Result<PathBuf> {
    let media_dir = store.root().join("media").join(adapter_id);
    fs::create_dir_all(&media_dir).await?;
    let file_name = staged_file_name(sandbox_path, attachment);
    let path = media_dir.join(format!("{}-{file_name}", Uuid7::now()));
    fs::write(&path, bytes)
        .await
        .with_context(|| format!("failed to stage adapter attachment {}", path.display()))?;
    Ok(path)
}

fn staged_file_name(sandbox_path: &str, attachment: &AdapterAttachment) -> String {
    let raw = attachment
        .file_name
        .as_deref()
        .or_else(|| {
            Path::new(sandbox_path)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .unwrap_or("attachment");
    let sanitized = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    }
}

fn not_found() -> ToolResult {
    serde_json::json!({
        "ok": false,
        "error": "adapter not found for this conversation",
    })
}

#[cfg(test)]
mod tests {
    use exoharness::{
        BasicExoHarness, ExoHarness, NewAgentRequest, NewConversationRequest, SandboxProvider,
        ToolRequest,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::test_support::local_test_config;
    use crate::{AgentConfig, AgentHarnessKind, ConversationConfig};

    #[tokio::test]
    async fn create_and_list_adapter_tools_are_conversation_scoped() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let create_result = execute_create_adapter_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "create_adapter",
                serde_json::json!({
                    "agentId": "spoofed-agent",
                    "conversationId": "spoofed-conversation",
                    "name": "irc",
                    "source": "built_in",
                    "config": {
                        "type": "irc",
                        "server": "irc.example.test",
                        "port": 6697,
                        "tls": true,
                        "nick": "exo",
                        "username": "exo",
                        "realname": "Exo",
                        "channel": "#exo",
                        "passwordSecretId": null,
                        "trigger": "mention"
                    }
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(create_result["ok"], true);
        let adapter_id = create_result["adapter"]["id"].as_str().unwrap();
        let adapter = store.get_adapter(adapter_id).await.unwrap().unwrap();
        assert_eq!(adapter.agent_id, agent.record().id.to_string());
        assert_eq!(
            adapter.conversation_id,
            conversation.record().id.to_string()
        );
        assert_eq!(adapter.config.worker_command[0], "pnpm");
        assert_eq!(adapter.config.worker_command[1], "tsx");
        assert!(
            adapter.config.worker_command[2].ends_with("examples/exoclaw/adapters/irc/worker.ts")
        );

        let list_result = execute_list_adapters_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_adapters",
                serde_json::json!({
                    "agentId": "spoofed-agent",
                    "conversationId": "spoofed-conversation",
                    "includeDisabled": false
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(list_result["adapters"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn agent_cli_config_applies_defaults() {
        let config: AdapterCreationConfig = serde_json::from_value(serde_json::json!({
            "type": "agent-cli",
            "socketPath": null,
            "mountRoot": "/Users/me/projects",
            "mountPath": null,
        }))
        .unwrap();
        assert_eq!(config.adapter_type(), "agent-cli");
        let adapter_config = config.into_adapter_config(AdapterSource::BuiltIn).unwrap();
        assert_eq!(adapter_config.adapter_type, "agent-cli");
        assert!(
            adapter_config.worker_command[2]
                .ends_with("examples/exoclaw/adapters/agent-cli/worker.ts")
        );
        assert_eq!(
            adapter_config.initialization,
            serde_json::json!({
                "socketPath": null,
                "mountRoot": "/Users/me/projects",
                "mountPath": "/agent-cli",
            })
        );
        assert!(adapter_config.secret_env.is_empty());
    }

    #[test]
    fn agent_cli_config_rejects_relative_mount_root_and_wrong_source() {
        let parse = |mount_root: &str| -> AdapterCreationConfig {
            serde_json::from_value(serde_json::json!({
                "type": "agent-cli",
                "socketPath": null,
                "mountRoot": mount_root,
                "mountPath": null,
            }))
            .unwrap()
        };
        let error = parse("projects")
            .into_adapter_config(AdapterSource::BuiltIn)
            .unwrap_err();
        assert!(error.to_string().contains("absolute host path"));
        let error = parse("/Users/me/projects")
            .into_adapter_config(AdapterSource::Library)
            .unwrap_err();
        assert!(error.to_string().contains("source"));
    }

    #[tokio::test]
    async fn list_adapter_events_is_conversation_scoped() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let owned = store
            .create_adapter(NewAdapter {
                agent_id: agent.record().id.to_string(),
                conversation_id: conversation.record().id.to_string(),
                name: "discord".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "discord".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();
        store
            .record_event(
                owned.id.clone(),
                AdapterEventType::Error,
                "shard error".to_string(),
            )
            .await
            .unwrap();
        let foreign = store
            .create_adapter(NewAdapter {
                agent_id: "other-agent".to_string(),
                conversation_id: "other-conversation".to_string(),
                name: "foreign".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "discord".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();

        let result = execute_list_adapter_events_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_adapter_events",
                serde_json::json!({
                    "adapterId": owned.id,
                    "eventType": "error",
                    "sinceMs": null,
                    "limit": null
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(result["ok"], true);
        let events = result["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["summary"], "shard error");

        let foreign_result = execute_list_adapter_events_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "list_adapter_events",
                serde_json::json!({
                    "adapterId": foreign.id,
                    "eventType": null,
                    "sinceMs": null,
                    "limit": null
                }),
            ),
        )
        .await
        .unwrap();
        assert_eq!(foreign_result["ok"], false);
    }

    #[tokio::test]
    async fn create_adapter_rejects_raw_worker_config() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));

        let error = execute_create_adapter_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &store,
            &tool_request(
                "create_adapter",
                serde_json::json!({
                    "name": "whatsapp",
                    "source": "library",
                    "config": {
                        "type": "whatsapp",
                        "authDir": null,
                        "trigger": "all_messages",
                        "allowedChats": null,
                        "workerCommand": ["sh", "-c", "cat /etc/passwd"]
                    }
                }),
            ),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("data did not match"));
        assert!(store.list_adapters().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn send_adapter_message_rejects_host_path_attachments() {
        let tempdir = TempDir::new().unwrap();
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        let agent = exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap();
        let conversation = agent
            .new_conversation(NewConversationRequest {
                slug: Some("conversation".to_string()),
                name: Some("Conversation".to_string()),
            })
            .await
            .unwrap();
        let store = AdapterStore::new(tempdir.path().join("adapters"));
        let adapter = store
            .create_adapter(NewAdapter {
                agent_id: agent.record().id.to_string(),
                conversation_id: conversation.record().id.to_string(),
                name: "whatsapp".to_string(),
                source: AdapterSource::Library,
                config: AdapterConfig {
                    adapter_type: "whatsapp".to_string(),
                    worker_command: vec!["pnpm".to_string(), "tsx".to_string()],
                    initialization: serde_json::json!({}),
                    state_dir: None,
                    secret_env: Vec::new(),
                },
            })
            .await
            .unwrap();

        let error = execute_send_adapter_message_tool(
            agent.as_ref(),
            conversation.as_ref(),
            &test_agent_config(),
            &ConversationConfig::default(),
            &store,
            &tool_request(
                "send_adapter_message",
                serde_json::json!({
                    "adapterId": adapter.id,
                    "text": "hello",
                    "target": "chat",
                    "attachments": [{
                        "kind": "image",
                        "path": "/etc/passwd",
                        "url": null,
                        "data": null,
                        "sandboxPath": null,
                        "mimeType": null,
                        "fileName": null
                    }]
                }),
            ),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("host-internal"));
    }

    fn tool_request(function_name: &str, arguments: serde_json::Value) -> ToolRequest {
        ToolRequest {
            function_name: function_name.to_string(),
            arguments: arguments.as_object().unwrap().clone(),
        }
    }

    fn test_agent_config() -> AgentConfig {
        AgentConfig {
            instructions: Vec::new(),
            harness: AgentHarnessKind::Exoclaw,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::Docker,
            enable_networking: false,
            model: "test-model".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        }
    }
}
