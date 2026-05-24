use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use exoharness::{AgentHandle, ConversationHandle, Secret, WriteArtifactRequest};
use serde::Serialize;

use super::store::AdapterStore;
use super::types::{
    AdapterBuildStatus, AdapterConfig, AdapterEventType, AdapterRecord, AdapterSource,
    WorkerAdapterConfig,
};
use super::worker::{WorkerCommand, WorkerEvent, run_worker_loop};
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
struct AdapterInboundArtifact {
    adapter_id: String,
    adapter_name: String,
    adapter_type: String,
    target: String,
    sender: Option<String>,
    text: String,
    message_id: Option<String>,
    metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
struct AdapterOutboundArtifact {
    adapter_id: String,
    adapter_name: String,
    adapter_type: String,
    target: Option<String>,
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct AdapterLifecycleArtifact {
    adapter_id: String,
    adapter_name: String,
    adapter_type: String,
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
    let AdapterConfig::Worker(config) = &adapter.config else {
        bail!("module-backed adapter sending is not available in this runtime yet");
    };
    let artifact = AdapterOutboundArtifact {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        adapter_type: config.adapter_type.clone(),
        target: target.map(ToOwned::to_owned),
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
            format!("queued {} adapter message", config.adapter_type),
            Some(artifact_version.artifact_id.to_string()),
        )
        .await?;
    store
        .enqueue_outbound_message(
            adapter.id.clone(),
            text.to_string(),
            target.map(ToOwned::to_owned),
        )
        .await?;
    Ok(())
}

async fn run_adapter_once(
    _harness: Arc<dyn Harness>,
    _store: &AdapterStore,
    adapter: AdapterRecord,
) -> Result<()> {
    bail!(
        "adapter `{}` requires the adapter watch runner",
        adapter.name
    )
}

async fn run_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    let AdapterConfig::Worker(config) = &adapter.config else {
        bail!("module-backed adapters do not have a host runner yet");
    };
    let secret_env = worker_secret_env(agent.exoharness_handle().as_ref(), config).await?;
    run_worker_loop(
        &adapter.id,
        config,
        secret_env,
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
                Ok(store
                    .take_outbound_messages(&adapter_id)
                    .await?
                    .into_iter()
                    .map(|message| WorkerCommand::SendMessage {
                        target: message.target,
                        text: message.text,
                    })
                    .collect())
            }
        },
    )
    .await
}

async fn handle_worker_event(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &WorkerAdapterConfig,
    event: WorkerEvent,
) -> Result<()> {
    match event {
        WorkerEvent::Connected { subject, metadata } => {
            store.mark_connected(&adapter.id).await?;
            record_worker_lifecycle(
                store,
                conversation,
                adapter,
                config,
                "connected",
                serde_json::json!({ "subject": subject, "metadata": metadata }),
            )
            .await
        }
        WorkerEvent::Message {
            target,
            sender,
            text,
            message_id,
            metadata,
        } => {
            handle_worker_message(
                store,
                conversation,
                adapter,
                config,
                target,
                sender,
                text,
                message_id,
                metadata,
            )
            .await
        }
        WorkerEvent::Lifecycle { name, metadata } => {
            record_worker_lifecycle(store, conversation, adapter, config, &name, metadata).await
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
                config,
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
    config: &WorkerAdapterConfig,
    target: String,
    sender: Option<String>,
    text: String,
    message_id: Option<String>,
    metadata: serde_json::Value,
) -> Result<()> {
    let artifact = AdapterInboundArtifact {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        adapter_type: config.adapter_type.clone(),
        target: target.clone(),
        sender: sender.clone(),
        text: text.clone(),
        message_id,
        metadata,
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
                "{} adapter message from {} to {}",
                config.adapter_type,
                sender.as_deref().unwrap_or("unknown"),
                target
            ),
            Some(artifact_version.artifact_id.to_string()),
        )
        .await?;
    send_conversation_wakeup(
        conversation,
        format!(
            "{} message received at target `{}` from {} via adapter `{}`:\n\n{}\n\nUse send_adapter_message with adapterId `{}` and target `{}` if you should reply externally. If this asks you to schedule future work whose results should be posted back externally, include this adapterId and target in the scheduled task reportPrompt.",
            config.adapter_type,
            target,
            sender.as_deref().unwrap_or("unknown"),
            adapter.name,
            text,
            adapter.id,
            target,
        ),
    )
    .await?;
    Ok(())
}

async fn record_worker_lifecycle(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &WorkerAdapterConfig,
    event_type: &str,
    payload: serde_json::Value,
) -> Result<()> {
    let artifact = AdapterLifecycleArtifact {
        adapter_id: adapter.id.clone(),
        adapter_name: adapter.name.clone(),
        adapter_type: config.adapter_type.clone(),
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
            format!("{} worker {event_type}", config.adapter_type),
            Some(artifact_version.artifact_id.to_string()),
        )
        .await?;
    Ok(())
}

fn adapter_source_ready(adapter: &AdapterRecord) -> bool {
    matches!(adapter.source, AdapterSource::BuiltIn)
        || matches!(
            adapter.build_status,
            AdapterBuildStatus::Succeeded | AdapterBuildStatus::NotRequired
        )
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

async fn worker_secret_env(
    agent: &dyn AgentHandle,
    config: &WorkerAdapterConfig,
) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for secret_env in &config.secret_env {
        let secret_uuid = secret_env.secret_id.parse()?;
        let Some(secret) = agent.get_secret(&secret_uuid).await? else {
            bail!("adapter secret not found: {}", secret_env.secret_id);
        };
        let value = match secret {
            Secret::Key { value } => value,
            Secret::Oauth { .. } => bail!("adapter worker secrets must be key secrets"),
        };
        env.push((secret_env.env.clone(), value));
    }
    Ok(env)
}
