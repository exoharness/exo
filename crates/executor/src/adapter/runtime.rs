use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use exoharness::{AgentHandle, ConversationHandle, Secret, SecretId};
use serde::Deserialize;
use tokio::sync::Notify;
use tokio::task::JoinSet;

use super::store::AdapterStore;
use super::types::{AdapterAttachment, AdapterConfig, AdapterEventType, AdapterRecord};
use super::worker::{WorkerCommand, WorkerEvent, run_worker_loop};
use crate::conversation_wakeup::send_conversation_wakeup;
use crate::{Harness, HarnessAgent, HarnessConversation};

const INITIAL_RESTART_DELAY: Duration = Duration::from_secs(5);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(300);
// A worker that survived this long was healthy; its next failure starts the
// backoff schedule over instead of inheriting a stale long delay.
const STABLE_RUN_THRESHOLD: Duration = Duration::from_secs(60);

// A reboot notice older than this is stale (e.g. left behind by a runner that
// crashed before claiming it) and should be dropped rather than announced.
const REBOOT_NOTICE_MAX_AGE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct AdapterRunOptions {
    pub limit: usize,
    /// When this file appears, the runner claims it (removes the file), stops
    /// starting new work, lets in-flight wakeup turns finish, and exits so a
    /// supervisor can restart it on a fresh build.
    pub drain_marker: Option<PathBuf>,
    /// Written by the service guardian when it restarts the adapter runner.
    /// A fresh runner claims it and wakes the adapter conversations so the
    /// agent can announce externally that it is back up.
    pub reboot_notice: Option<PathBuf>,
}

impl Default for AdapterRunOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            drain_marker: None,
            reboot_notice: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RebootNotice {
    #[serde(default)]
    pub requested_at: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

pub async fn run_adapters_watch(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    options: AdapterRunOptions,
) -> Result<()> {
    let running = Arc::new(Mutex::new(HashSet::<String>::new()));
    let drain = Arc::new(AtomicBool::new(false));
    let mut supervisors = JoinSet::new();
    if let Some(notice) = claim_reboot_notice(options.reboot_notice.as_deref()) {
        // The wakeup turn can take minutes; run it in the background so the
        // adapter workers (and the connections the agent will announce on)
        // start immediately. The durable outbox holds any announcement until
        // the relevant worker is connected.
        let harness = Arc::clone(&harness);
        let store = store.clone();
        tokio::spawn(async move {
            if let Err(error) = announce_reboot(harness, store, notice).await {
                tracing::error!(%error, "failed to announce adapter runner reboot");
            }
        });
    }
    loop {
        if claim_drain_marker(options.drain_marker.as_deref()) {
            tracing::info!("adapter runner drain requested; waiting for in-flight work");
            drain.store(true, Ordering::SeqCst);
            break;
        }
        let adapters = store.enabled_adapters().await?;
        for adapter in adapters.into_iter().take(options.limit) {
            if !running
                .lock()
                .expect("adapter running set poisoned")
                .insert(adapter.id.clone())
            {
                continue;
            }
            let harness = Arc::clone(&harness);
            let store = store.clone();
            let running = Arc::clone(&running);
            let drain = Arc::clone(&drain);
            supervisors.spawn(async move {
                let adapter_id = adapter.id.clone();
                supervise_adapter(harness, store, adapter, drain).await;
                running
                    .lock()
                    .expect("adapter running set poisoned")
                    .remove(&adapter_id);
            });
        }
        // Reap finished supervision tasks so the JoinSet does not grow
        // unboundedly while the runner stays up.
        while supervisors.try_join_next().is_some() {}
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    while supervisors.join_next().await.is_some() {}
    tracing::info!("adapter runner drained; exiting for restart");
    Ok(())
}

fn claim_drain_marker(marker: Option<&std::path::Path>) -> bool {
    let Some(marker) = marker else {
        return false;
    };
    std::fs::remove_file(marker).is_ok()
}

fn claim_reboot_notice(notice_path: Option<&std::path::Path>) -> Option<RebootNotice> {
    let notice_path = notice_path?;
    let contents = std::fs::read_to_string(notice_path).ok()?;
    let age = std::fs::metadata(notice_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok());
    if std::fs::remove_file(notice_path).is_err() {
        // Another claimant got there first.
        return None;
    }
    match age {
        Some(age) if age <= REBOOT_NOTICE_MAX_AGE => {}
        _ => {
            tracing::warn!(
                path = %notice_path.display(),
                "ignoring stale reboot notice"
            );
            return None;
        }
    }
    match serde_json::from_str::<RebootNotice>(&contents) {
        Ok(notice) => Some(notice),
        Err(error) => {
            tracing::warn!(
                path = %notice_path.display(),
                %error,
                "failed to parse reboot notice"
            );
            None
        }
    }
}

async fn announce_reboot(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    notice: RebootNotice,
) -> Result<()> {
    let adapters = store.enabled_adapters().await?;
    let mut woken = HashSet::new();
    let reason = notice.reason.as_deref().unwrap_or("unspecified");
    let requested_at = notice.requested_at.as_deref().unwrap_or("unknown time");
    for adapter in &adapters {
        if !woken.insert((adapter.agent_id.clone(), adapter.conversation_id.clone())) {
            continue;
        }
        let adapter_names = adapters
            .iter()
            .filter(|candidate| {
                candidate.agent_id == adapter.agent_id
                    && candidate.conversation_id == adapter.conversation_id
            })
            .map(|candidate| format!("`{}` ({})", candidate.name, candidate.config.adapter_type))
            .collect::<Vec<_>>()
            .join(", ");
        let agent = require_agent(harness.as_ref(), adapter).await?;
        let conversation = require_conversation(agent.as_ref(), adapter).await?;
        send_conversation_wakeup(
            conversation.as_ref(),
            format!(
                "Host services were restarted (reason: {reason}, requested at {requested_at}) and the adapter runner is back up. Adapter workers for {adapter_names} are reconnecting now. If you announced this reboot externally, or external users should know you are back, announce your return with send_adapter_message on the relevant adapters and targets; outbound messages queue durably and deliver once the adapter reconnects. If no announcement is appropriate, do nothing.",
            ),
        )
        .await
        .with_context(|| {
            format!(
                "reboot announcement wakeup failed for conversation {}",
                adapter.conversation_id
            )
        })?;
    }
    Ok(())
}

async fn supervise_adapter(
    harness: Arc<dyn Harness>,
    store: AdapterStore,
    adapter: AdapterRecord,
    drain: Arc<AtomicBool>,
) {
    let mut restart_delay = INITIAL_RESTART_DELAY;
    loop {
        if drain.load(Ordering::SeqCst) {
            break;
        }
        match store.get_adapter(&adapter.id).await {
            Ok(Some(current)) if current.enabled => {}
            Ok(Some(_)) => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            Ok(_) => break,
            Err(error) => {
                tracing::error!(
                    adapter_id = %adapter.id,
                    %error,
                    "failed to read adapter record; retrying"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        }
        let started_at = Instant::now();
        if let Err(error) = run_adapter_loop(
            Arc::clone(&harness),
            &store,
            adapter.clone(),
            Arc::clone(&drain),
        )
        .await
        {
            if started_at.elapsed() >= STABLE_RUN_THRESHOLD {
                restart_delay = INITIAL_RESTART_DELAY;
            }
            tracing::error!(
                adapter_id = %adapter.id,
                %error,
                "adapter runtime error; restarting in {}s",
                restart_delay.as_secs()
            );
            if let Err(mark_error) = store.mark_error(&adapter.id, error.to_string()).await {
                tracing::error!(
                    adapter_id = %adapter.id,
                    error = %mark_error,
                    "failed to mark adapter error"
                );
            }
            if let Err(record_error) = store
                .record_event(
                    adapter.id.clone(),
                    AdapterEventType::Error,
                    error.to_string(),
                )
                .await
            {
                tracing::error!(
                    adapter_id = %adapter.id,
                    error = %record_error,
                    "failed to record adapter error"
                );
            }
            tokio::time::sleep(restart_delay).await;
            restart_delay = (restart_delay * 2).min(MAX_RESTART_DELAY);
            continue;
        }
        restart_delay = INITIAL_RESTART_DELAY;
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn send_adapter_message_with_handles(
    _agent: &dyn AgentHandle,
    _conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    adapter: &AdapterRecord,
    text: &str,
    target: Option<&str>,
    attachments: Vec<AdapterAttachment>,
) -> Result<()> {
    if !adapter.enabled {
        bail!("adapter is disabled: {}", adapter.id);
    }
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
    // Note: we intentionally do not write a conversation artifact here.
    // This tool is invoked from inside an active agent turn, and writing to
    // the conversation outside the turn handle advances the conversation
    // head, which makes the active turn stale and crashes the adapter
    // worker. The outbound message is durably queued in the AdapterStore
    // outbox, and the event below records it for audit.
    store
        .record_event(
            adapter.id.clone(),
            AdapterEventType::Outbound,
            format!(
                "queued {} adapter message{}",
                adapter.config.adapter_type,
                target.map(|t| format!(" to {t}")).unwrap_or_default(),
            ),
        )
        .await?;
    store
        .enqueue_outbound_message(
            adapter.id.clone(),
            text.to_string(),
            target.map(ToOwned::to_owned),
            attachments,
        )
        .await?;
    notify_adapter_outbound(&adapter.id);
    Ok(())
}

async fn run_adapter_loop(
    harness: Arc<dyn Harness>,
    store: &AdapterStore,
    adapter: AdapterRecord,
    drain: Arc<AtomicBool>,
) -> Result<()> {
    let agent = require_agent(harness.as_ref(), &adapter).await?;
    let conversation = require_conversation(agent.as_ref(), &adapter).await?;
    store.requeue_inflight_messages(&adapter.id).await?;
    let config = adapter.config.clone();
    let secret_env = worker_secret_env(agent.exoharness_handle().as_ref(), &config).await?;
    let outbound_notifier = register_adapter_outbound_notifier(&adapter.id);
    let event_store = store.clone();
    let event_adapter = adapter.clone();
    let event_conversation = std::sync::Arc::clone(&conversation);
    let event_config = config.clone();
    let outbound_store = store.clone();
    let outbound_adapter_id = adapter.id.clone();
    let stop_store = store.clone();
    let stop_adapter_id = adapter.id.clone();
    run_worker_loop(
        &adapter.id,
        &config,
        secret_env,
        Arc::clone(&outbound_notifier.notify),
        move |event| {
            let store = event_store.clone();
            let adapter = event_adapter.clone();
            let conversation = std::sync::Arc::clone(&event_conversation);
            let config = event_config.clone();
            async move {
                handle_worker_event(&store, conversation.as_ref(), &adapter, &config, event).await
            }
        },
        move || {
            let store = outbound_store.clone();
            let adapter_id = outbound_adapter_id.clone();
            async move {
                Ok(store
                    .claim_outbound_messages(&adapter_id)
                    .await?
                    .into_iter()
                    .map(|message| WorkerCommand::SendMessage {
                        id: message.id,
                        target: message.target,
                        text: message.text,
                        attachments: message.attachments,
                    })
                    .collect())
            }
        },
        move || {
            let store = stop_store.clone();
            let adapter_id = stop_adapter_id.clone();
            let drain = Arc::clone(&drain);
            async move {
                if drain.load(Ordering::SeqCst) {
                    return Ok(true);
                }
                Ok(store
                    .get_adapter(&adapter_id)
                    .await?
                    .is_none_or(|adapter| !adapter.enabled))
            }
        },
    )
    .await
}

struct AdapterOutboundNotifierGuard {
    adapter_id: String,
    notify: Arc<Notify>,
}

impl Drop for AdapterOutboundNotifierGuard {
    fn drop(&mut self) {
        let mut notifiers = adapter_outbound_notifiers()
            .lock()
            .expect("adapter outbound notifier registry poisoned");
        if let Some(current) = notifiers.get(&self.adapter_id).and_then(Weak::upgrade)
            && Arc::ptr_eq(&current, &self.notify)
        {
            notifiers.remove(&self.adapter_id);
        }
    }
}

fn register_adapter_outbound_notifier(adapter_id: &str) -> AdapterOutboundNotifierGuard {
    let notify = Arc::new(Notify::new());
    adapter_outbound_notifiers()
        .lock()
        .expect("adapter outbound notifier registry poisoned")
        .insert(adapter_id.to_string(), Arc::downgrade(&notify));
    AdapterOutboundNotifierGuard {
        adapter_id: adapter_id.to_string(),
        notify,
    }
}

fn notify_adapter_outbound(adapter_id: &str) {
    let notify = adapter_outbound_notifiers()
        .lock()
        .expect("adapter outbound notifier registry poisoned")
        .get(adapter_id)
        .and_then(Weak::upgrade);
    if let Some(notify) = notify {
        notify.notify_one();
    }
}

fn adapter_outbound_notifiers() -> &'static Mutex<HashMap<String, Weak<Notify>>> {
    static NOTIFIERS: OnceLock<Mutex<HashMap<String, Weak<Notify>>>> = OnceLock::new();
    NOTIFIERS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn handle_worker_event(
    store: &AdapterStore,
    conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
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
                .record_event(adapter.id.clone(), AdapterEventType::Error, message)
                .await?;
            Ok(())
        }
        WorkerEvent::CommandAck { command_id } => {
            store
                .acknowledge_outbound_message(&adapter.id, &command_id)
                .await
        }
        WorkerEvent::CommandNack {
            command_id,
            message,
        } => {
            store.mark_error(&adapter.id, message.clone()).await?;
            store
                .record_event(adapter.id.clone(), AdapterEventType::Error, message)
                .await?;
            store
                .acknowledge_outbound_message(&adapter.id, &command_id)
                .await
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
    config: &AdapterConfig,
    target: String,
    sender: Option<String>,
    text: String,
    message_id: Option<String>,
    _metadata: serde_json::Value,
) -> Result<()> {
    if let Some(message_id) = &message_id
        && !store
            .record_inbound_message_once(&adapter.id, &target, message_id)
            .await?
    {
        return Ok(());
    }
    // Note: we intentionally do not write a conversation artifact here.
    // The wakeup turn below begins immediately, and any artifact writes
    // through the conversation handle (rather than the active turn) advance
    // the conversation head and could race with concurrent turns. The full
    // inbound text is delivered to the agent via the wakeup prompt and the
    // event is recorded in the AdapterStore for audit.
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
        )
        .await?;
    let wakeup_result = send_conversation_wakeup(
        conversation,
        format!(
            "{} message received at target `{}` from {} via adapter `{}`:\n\n{}\n\nThis message came from an external adapter. If you answer this message, you MUST reply externally with send_adapter_message using adapterId `{}` and target `{}`. Do not answer only in the REPL unless you are explicitly deciding that no external reply should be sent. If this asks you to schedule future work whose results should be posted back externally, include adapterId `{}` and target `{}` in the scheduled task reportPrompt.",
            config.adapter_type,
            target,
            sender.as_deref().unwrap_or("unknown"),
            adapter.name,
            text,
            adapter.id,
            target,
            adapter.id,
            target,
        ),
    )
    .await;
    // A failed model turn must not tear down the worker: the external
    // connection is healthy and dropping it loses every queued message.
    // Record the failure and keep processing events.
    if let Err(error) = wakeup_result {
        tracing::error!(
            adapter_id = %adapter.id,
            %error,
            "adapter wakeup turn failed; worker stays up"
        );
        store
            .mark_error(&adapter.id, format!("wakeup turn failed: {error}"))
            .await?;
        store
            .record_event(
                adapter.id.clone(),
                AdapterEventType::Error,
                format!("wakeup turn failed: {error}"),
            )
            .await?;
    }
    Ok(())
}

async fn record_worker_lifecycle(
    store: &AdapterStore,
    _conversation: &dyn HarnessConversation,
    adapter: &AdapterRecord,
    config: &AdapterConfig,
    event_type: &str,
    _payload: serde_json::Value,
) -> Result<()> {
    // Note: lifecycle events are recorded only in the AdapterStore. Writing
    // them to the conversation as artifacts can advance the head outside of
    // any active turn and corrupt the agent's turn state.
    store
        .record_event(
            adapter.id.clone(),
            match event_type {
                "connected" => AdapterEventType::Connected,
                "disconnected" => AdapterEventType::Disconnected,
                "error" => AdapterEventType::Error,
                _ => AdapterEventType::Lifecycle,
            },
            format!("{} worker {event_type}", config.adapter_type),
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

async fn worker_secret_env(
    agent: &dyn AgentHandle,
    config: &AdapterConfig,
) -> Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    for secret_env in &config.secret_env {
        let secret_uuid = resolve_secret_id(agent, &secret_env.secret_id).await?;
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

async fn resolve_secret_id(agent: &dyn AgentHandle, reference: &str) -> Result<SecretId> {
    if let Ok(secret_id) = reference.parse() {
        return Ok(secret_id);
    }
    agent
        .list_secrets()
        .await?
        .into_iter()
        .find(|secret| secret.name == reference)
        .map(|secret| secret.id)
        .ok_or_else(|| anyhow!("adapter secret not found: {reference}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_and_parses_fresh_reboot_notice() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let path = tempdir.path().join("exoclaw-reboot-notice.json");
        std::fs::write(
            &path,
            r#"{"requestedAt":"2026-06-09T19:39:02Z","reason":"restart-all"}"#,
        )
        .unwrap();

        let notice = claim_reboot_notice(Some(&path)).expect("notice should be claimed");
        assert_eq!(notice.reason.as_deref(), Some("restart-all"));
        assert_eq!(notice.requested_at.as_deref(), Some("2026-06-09T19:39:02Z"));
        assert!(!path.exists(), "claiming must remove the notice file");

        assert!(claim_reboot_notice(Some(&path)).is_none());
    }

    #[test]
    fn ignores_missing_and_malformed_reboot_notices() {
        let tempdir = tempfile::TempDir::new().unwrap();
        let path = tempdir.path().join("exoclaw-reboot-notice.json");
        assert!(claim_reboot_notice(Some(&path)).is_none());
        assert!(claim_reboot_notice(None).is_none());

        std::fs::write(&path, "not json").unwrap();
        assert!(claim_reboot_notice(Some(&path)).is_none());
        assert!(!path.exists(), "malformed notices are still consumed");
    }
}
