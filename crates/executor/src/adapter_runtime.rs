use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use exoharness::{AgentHandle, ConversationHandle, Secret, WriteArtifactRequest};
use serde::Serialize;

use crate::adapter_irc::{
    IrcPrivateMessage, run_irc_adapter_loop_with_connected, run_irc_adapter_once_with_connected,
};
use crate::adapter_store::AdapterStore;
use crate::adapter_types::{
    AdapterBuildStatus, AdapterConfig, AdapterEventType, AdapterRecord, AdapterSource,
    IrcAdapterConfig, WhatsappAdapterConfig, WhatsappTriggerPolicy,
};
use crate::adapter_worker::{WorkerCommand, WorkerEvent, run_whatsapp_worker_loop};
use crate::conversation_wakeup::send_conversation_wakeup;
use crate::{Harness, HarnessAgent, HarnessConversation};

#[derive(Debug, Clone, Copy)]
pub struct AdapterRunOptions {
    pub limit: usize,
}

impl Default for AdapterRunOptions {
    fn default() -> Self {
        Self { limit: 10 }
    }
}

#[derive(Debug, Clone, Serialize)]
struct IrcInboundArtifact {
    adapter_id: String,
    adapter_name: String,
    server: String,
    channel: String,
    nick: String,
    text: String,
    raw: String,
}

#[derive(Debug, Clone, Serialize)]
struct IrcOutboundArtifact {
    adapter_id: String,
    adapter_name: String,
    server: String,
    channel: String,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerInboundArtifact {
    adapter_id: String,
    adapter_name: String,
    chat_id: String,
    sender: Option<String>,
    text: String,
    message_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkerLifecycleArtifact {
    adapter_id: String,
    adapter_name: String,
    event_type: String,
    payload: serde_json::Value,
}

pub async fn run_adapters_once(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    options: AdapterRunOptions,
) -> Result<usize> {
    let mut adapters = store.enabled_adapters().await?;
    adapters.truncate(options.limit);
    let mut handled = 0;
    for adapter in adapters {
        if !adapter_source_ready(&adapter) {
            continue;
        }
        if run_adapter_once(Arc::clone(&harness), store, adapter)
            .await
            .is_ok()
        {
            handled += 1;
        }
    }
    Ok(handled)
}

pub async fn run_adapters_watch(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    options: AdapterRunOptions,
) -> Result<()> {
    let mut running = HashSet::new();
    loop {
        let adapters = store.enabled_adapters().await?;
        for adapter in adapters.into_iter().take(options.limit) {
            if !running.insert(adapter.id.clone()) {
                continue;
            }
            let harness = Arc::clone(&harness);
            let store = store.clone();
            tokio::spawn(async move {
                loop {
                    match store.get_adapter(&adapter.id).await {
                        Ok(Some(current)) if current.enabled && adapter_source_ready(&current) => {}
                        Ok(Some(_)) => {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            continue;
                        }
                        Ok(_) => break,
                        Err(_) => break,
                    }
                    if let Err(error) =
                        run_adapter_loop(Arc::clone(&harness), &store, adapter.clone()).await
                    {
                        let _ = store.mark_error(&adapter.id, error.to_string()).await;
                        let _ = store
                            .record_event(
                                adapter.id.clone(),
                                AdapterEventType::Error,
                                error.to_string(),
                                None,
                            )
                            .await;
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            });
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

pub async fn send_adapter_message_with_handles(
    _agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    adapter: &AdapterRecord,
    text: &str,
    target: Option<&str>,
) -> Result<()> {
    if !adapter.enabled {
        bail!("adapter is disabled: {}", adapter.id);
    }
    if !adapter_source_ready(adapter) {
        bail!("adapter has not been built successfully: {}", adapter.id);
    }
    match &adapter.config {
        AdapterConfig::Irc(config) => {
            let artifact = IrcOutboundArtifact {
                adapter_id: adapter.id.clone(),
                adapter_name: adapter.name.clone(),
                server: config.server.clone(),
                channel: config.channel.clone(),
                text: text.to_string(),
            };
            let artifact_version = conversation
                .write_artifact(WriteArtifactRequest {
                    path: format!(
                        "adapters/{}/outbound-{}.json",
                        adapter.name,
                        crate::Uuid7::now()
                    ),
                    contents: serde_json::to_vec_pretty(&artifact)?,
                })
                .await?;
            store
                .record_event(
                    adapter.id.clone(),
                    AdapterEventType::Outbound,
                    format!("sent IRC message to {}", config.channel),
                    Some(artifact_version.artifact_id.to_string()),
                )
                .await?;
            store
                .enqueue_outbound_message(adapter.id.clone(), text.to_string(), None)
                .await?;
            Ok(())
        }
        AdapterConfig::Whatsapp(_) => {
            let target = target
                .ok_or_else(|| anyhow!("WhatsApp adapter messages require a target chat id"))?;
            let artifact = serde_json::json!({
                "adapter_id": adapter.id,
                "adapter_name": adapter.name,
                "target": target,
                "text": text,
            });
            let artifact_version = conversation
                .write_artifact(WriteArtifactRequest {
                    path: format!(
                        "adapters/{}/outbound-{}.json",
                        adapter.name,
                        crate::Uuid7::now()
                    ),
                    contents: serde_json::to_vec_pretty(&artifact)?,
                })
                .await?;
            store
                .record_event(
                    adapter.id.clone(),
                    AdapterEventType::Outbound,
                    "queued WhatsApp message".to_string(),
                    Some(artifact_version.artifact_id.to_string()),
                )
                .await?;
            store
                .enqueue_outbound_message(
                    adapter.id.clone(),
                    text.to_string(),
                    Some(target.to_string()),
                )
                .await?;
            Ok(())
        }
        AdapterConfig::Module(_) => {
            bail!("module-backed adapter sending is not available in this runtime yet")
        }
    }
}

async fn run_adapter_once(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    let password = adapter_password(agent.as_ref(), &adapter).await?;
    match &adapter.config {
        AdapterConfig::Irc(config) => {
            let message = run_irc_adapter_once_with_connected(config, password.as_deref(), || {
                let store = store.clone();
                let adapter_id = adapter.id.clone();
                async move {
                    store.mark_connected(&adapter_id).await?;
                    store
                        .record_event(
                            adapter_id,
                            AdapterEventType::Connected,
                            "connected IRC adapter".to_string(),
                            None,
                        )
                        .await?;
                    Ok(())
                }
            })
            .await?;
            if !store
                .get_adapter(&adapter.id)
                .await?
                .is_some_and(|adapter| adapter.enabled)
            {
                return Ok(());
            }
            handle_irc_message(store, conversation.as_ref(), &adapter, config, message).await
        }
        AdapterConfig::Whatsapp(config) => {
            run_whatsapp_adapter_once(harness, store, &adapter, config).await
        }
        AdapterConfig::Module(_) => {
            bail!(
                "module-backed adapters can be built and registered but do not have a host runner yet"
            )
        }
    }
}

async fn run_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    let password = adapter_password(agent.as_ref(), &adapter).await?;
    match &adapter.config {
        AdapterConfig::Irc(config) => {
            run_irc_adapter_loop_with_connected(
                config,
                password.as_deref(),
                || {
                    let store = store.clone();
                    let adapter_id = adapter.id.clone();
                    async move {
                        store.mark_connected(&adapter_id).await?;
                        store
                            .record_event(
                                adapter_id,
                                AdapterEventType::Connected,
                                "connected IRC adapter".to_string(),
                                None,
                            )
                            .await?;
                        Ok(())
                    }
                },
                |message| {
                    let store = store.clone();
                    let adapter = adapter.clone();
                    let conversation = std::sync::Arc::clone(&conversation);
                    let config = config.clone();
                    async move {
                        if !store
                            .get_adapter(&adapter.id)
                            .await?
                            .is_some_and(|adapter| adapter.enabled)
                        {
                            return Ok(());
                        }
                        handle_irc_message(
                            &store,
                            conversation.as_ref(),
                            &adapter,
                            &config,
                            message,
                        )
                        .await
                    }
                },
                || {
                    let store = store.clone();
                    let adapter_id = adapter.id.clone();
                    async move {
                        Ok(store
                            .take_outbound_messages(&adapter_id)
                            .await?
                            .into_iter()
                            .map(|message| message.text)
                            .collect())
                    }
                },
            )
            .await
        }
        AdapterConfig::Whatsapp(config) => {
            run_whatsapp_adapter_loop(harness, store, &adapter, config).await
        }
        AdapterConfig::Module(_) => {
            bail!(
                "module-backed adapters can be built and registered but do not have a host runner yet"
            )
        }
    }
}

async fn run_whatsapp_adapter_once(
    _harness: Arc<dyn Harness>,
    _store: &AdapterStore,
    adapter: &AdapterRecord,
    _config: &WhatsappAdapterConfig,
) -> Result<()> {
    bail!(
        "WhatsApp adapter `{}` requires the adapter watch runner",
        adapter.name
    )
}

async fn run_whatsapp_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: &AdapterRecord,
    config: &WhatsappAdapterConfig,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), adapter).await?;
    let conversation = require_conversation(agent.as_ref(), adapter).await?;
    run_whatsapp_worker_loop(
        &adapter.id,
        config,
        |event| {
            let store = store.clone();
            let adapter = adapter.clone();
            let conversation = std::sync::Arc::clone(&conversation);
            let config = config.clone();
            async move {
                handle_worker_event(&store, conversation.as_ref(), &adapter, &config, event).await
            }
        },
        || {
            let store = store.clone();
            let adapter_id = adapter.id.clone();
            async move {
                let mut commands = Vec::new();
                for message in store.take_outbound_messages(&adapter_id).await? {
                    let Some(target) = message.target else {
                        continue;
                    };
                    commands.push(WorkerCommand::SendMessage {
                        target,
                        text: message.text,
                    });
                }
                Ok(commands)
            }
        },
    )
    .await
}

async fn handle_worker_event(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &WhatsappAdapterConfig,
    event: WorkerEvent,
) -> Result<()> {
    match event {
        WorkerEvent::Qr { qr } => {
            record_worker_lifecycle(
                store,
                conversation,
                adapter,
                "qr",
                serde_json::json!({ "qr": qr }),
            )
            .await
        }
        WorkerEvent::Connected { jid } => {
            store.mark_connected(&adapter.id).await?;
            record_worker_lifecycle(
                store,
                conversation,
                adapter,
                "connected",
                serde_json::json!({ "jid": jid }),
            )
            .await
        }
        WorkerEvent::Message {
            chat_id,
            sender,
            text,
            message_id,
        } => {
            if !whatsapp_should_trigger(config, &chat_id) {
                return Ok(());
            }
            handle_worker_message(
                store,
                conversation,
                adapter,
                chat_id,
                sender,
                text,
                message_id,
            )
            .await
        }
        WorkerEvent::Error { message } => {
            store.mark_error(&adapter.id, message.clone()).await?;
            store
                .record_event(adapter.id.clone(), AdapterEventType::Error, message, None)
                .await?;
            Ok(())
        }
        WorkerEvent::Disconnected { reason } => {
            record_worker_lifecycle(
                store,
                conversation,
                adapter,
                "disconnected",
                serde_json::json!({ "reason": reason }),
            )
            .await
        }
    }
}

async fn handle_worker_message(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    chat_id: String,
    sender: Option<String>,
    text: String,
    message_id: Option<String>,
) -> Result<()> {
    let artifact = WorkerInboundArtifact {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        chat_id: chat_id.clone(),
        sender: sender.clone(),
        text: text.clone(),
        message_id,
    };
    let artifact_version = conversation
        .exoharness_handle()
        .write_artifact(WriteArtifactRequest {
            path: format!(
                "adapters/{}/inbound-{}.json",
                adapter.name,
                crate::Uuid7::now()
            ),
            contents: serde_json::to_vec_pretty(&artifact)?,
        })
        .await?;
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Inbound,
            format!(
                "WhatsApp message from {} in {}",
                sender.as_deref().unwrap_or("unknown"),
                chat_id
            ),
            Some(artifact_version.artifact_id.to_string()),
        )
        .await?;
    send_conversation_wakeup(
        conversation,
        format!(
            "WhatsApp message received in chat `{}` from {} via adapter `{}`:\n\n{}\n\nUse send_adapter_message with adapterId `{}` and target `{}` if you should reply to WhatsApp. If this asks you to schedule future work whose results should be posted back to WhatsApp, include this adapterId and target in the scheduled task reportPrompt.",
            chat_id,
            sender.as_deref().unwrap_or("unknown"),
            adapter.name,
            text,
            adapter.id,
            chat_id,
        ),
    )
    .await?;
    Ok(())
}

async fn record_worker_lifecycle(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let artifact = WorkerLifecycleArtifact {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        event_type: event_type.to_string(),
        payload,
    };
    let artifact_version = conversation
        .exoharness_handle()
        .write_artifact(WriteArtifactRequest {
            path: format!(
                "adapters/{}/{}-{}.json",
                adapter.name,
                event_type,
                crate::Uuid7::now()
            ),
            contents: serde_json::to_vec_pretty(&artifact)?,
        })
        .await?;
    store
        .record_event(
            adapter.id.clone(),
            match event_type {
                "connected" => AdapterEventType::Connected,
                "error" => AdapterEventType::Error,
                _ => AdapterEventType::Inbound,
            },
            format!("WhatsApp worker {event_type}"),
            Some(artifact_version.artifact_id.to_string()),
        )
        .await?;
    Ok(())
}

fn whatsapp_should_trigger(config: &WhatsappAdapterConfig, chat_id: &str) -> bool {
    if let Some(allowed_chats) = &config.allowed_chats
        && !allowed_chats.iter().any(|allowed| allowed == chat_id)
    {
        return false;
    }
    match config.trigger {
        WhatsappTriggerPolicy::AllMessages => true,
        WhatsappTriggerPolicy::ContactsOnly => !chat_id.ends_with("@g.us"),
    }
}

fn adapter_source_ready(adapter: &AdapterRecord) -> bool {
    matches!(adapter.source, AdapterSource::BuiltIn)
        || matches!(
            adapter.build_status,
            AdapterBuildStatus::Succeeded | AdapterBuildStatus::NotRequired
        )
}

async fn handle_irc_message(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &IrcAdapterConfig,
    message: IrcPrivateMessage,
) -> Result<()> {
    let artifact = IrcInboundArtifact {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        server: config.server.clone(),
        channel: config.channel.clone(),
        nick: message.nick.clone(),
        text: message.text.clone(),
        raw: message.raw.clone(),
    };
    let artifact_version = conversation
        .exoharness_handle()
        .write_artifact(WriteArtifactRequest {
            path: format!(
                "adapters/{}/inbound-{}.json",
                adapter.name,
                crate::Uuid7::now()
            ),
            contents: serde_json::to_vec_pretty(&artifact)?,
        })
        .await?;
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Inbound,
            format!("IRC message from {} in {}", message.nick, config.channel),
            Some(artifact_version.artifact_id.to_string()),
        )
        .await?;
    send_conversation_wakeup(
        conversation,
        format!(
            "IRC message received in {} from {} via adapter `{}`:\n\n{}\n\nUse send_adapter_message with adapterId `{}` if you should reply to IRC. If this IRC message asks you to schedule recurring or future work whose results should be posted back to IRC, include that instruction in the scheduled task reportPrompt, including this adapterId.",
            config.channel, message.nick, adapter.name, message.text, adapter.id
        ),
    )
    .await?;
    Ok(())
}

async fn require_agent(
    harness: &dyn Harness,
    adapter: &AdapterRecord,
) -> Result<Arc<dyn HarnessAgent>> {
    harness
        .get_agent(&adapter.agent_id)
        .await?
        .ok_or_else(|| anyhow!("adapter agent does not exist: {}", adapter.agent_id))
}

async fn require_conversation(
    agent: &dyn HarnessAgent,
    adapter: &AdapterRecord,
) -> Result<Arc<dyn HarnessConversation>> {
    agent
        .get_conversation(&adapter.conversation_id)
        .await?
        .ok_or_else(|| {
            anyhow!(
                "adapter conversation does not exist: {}",
                adapter.conversation_id
            )
        })
}

async fn adapter_password(
    agent: &dyn HarnessAgent,
    adapter: &AdapterRecord,
) -> Result<Option<String>> {
    adapter_password_from_handle(agent.exoharness_handle().as_ref(), adapter).await
}

async fn adapter_password_from_handle(
    agent: &dyn AgentHandle,
    adapter: &AdapterRecord,
) -> Result<Option<String>> {
    let AdapterConfig::Irc(config) = &adapter.config else {
        return Ok(None);
    };
    let Some(secret_id) = &config.password_secret_id else {
        return Ok(None);
    };
    let secret_uuid = secret_id.parse()?;
    let Some(secret) = agent.get_secret(&secret_uuid).await? else {
        bail!("IRC password secret not found: {secret_id}");
    };
    match secret {
        Secret::Key { value } => Ok(Some(value)),
        Secret::Oauth { .. } => bail!("IRC password secret must be a key secret"),
    }
}
