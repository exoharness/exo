use super::{
    AdapterService, Deployment, progress,
    store::{AdapterStore, DeliveryKind, IncomingMessage, Outbox, Route},
};
use anyhow::{Context, Result, ensure};
use exo_managed_agents::adapters::{ChannelTrigger, SlackAdapter, SlackTransport};
use serde::{Deserialize, Serialize};
use slack_morphism::prelude::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

pub struct SlackConnection {
    pub account: String,
    pub config: SlackAdapter,
    client: Arc<SlackHyperClient>,
    token: SlackApiToken,
    team: SlackTeamId,
    enterprise: Option<SlackEnterpriseId>,
    user: SlackUserId,
    app: String,
    socket: Mutex<
        Option<
            SlackClientSocketModeListener<
                slack_morphism::hyper_tokio::SlackClientHyperHttpsConnector,
            >,
        >,
    >,
}

#[derive(Clone)]
struct SocketContext {
    service: Weak<AdapterService>,
    deployment: String,
}

// The SDK currently omits is_ext_shared from its conversations.info model.
// Use its HTTP client with this additional typed response for admission policy.
#[derive(Deserialize)]
struct ChannelPrivacyResponse {
    channel: ChannelPrivacy,
}
#[derive(Deserialize)]
struct ChannelPrivacy {
    is_ext_shared: Option<bool>,
    is_im: Option<bool>,
    is_mpim: Option<bool>,
}

// auth.test includes the installation's enterprise ID, which the SDK omits.
#[derive(Deserialize)]
struct AuthResponse {
    #[serde(flatten)]
    auth: SlackApiAuthTestResponse,
    enterprise_id: Option<SlackEnterpriseId>,
}

// The SDK omits Slack's custom loading text from its setStatus request.
#[derive(Serialize, Deserialize)]
pub(super) struct ThreadStatusRequest {
    #[serde(flatten)]
    pub status: SlackApiAssistantThreadsSetStatusRequest,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub loading_messages: Vec<String>,
}

// The SDK's HTTP and websocket connectors use Rustls's process default.
// Both ring and aws-lc are linked in Exo; choose ring only if the host has not
// already selected a provider. Another caller may win installation concurrently.
pub(super) fn initialize_tls() -> Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none()
        && rustls::crypto::ring::default_provider()
            .install_default()
            .is_err()
    {
        ensure!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "unable to initialize Slack TLS"
        );
    }
    Ok(())
}

impl SlackConnection {
    fn internal_user(&self, user: &SlackUser) -> Result<bool> {
        if let (Some(enterprise), Some(member)) = (&self.enterprise, &user.enterprise_user)
            && member.enterprise_id == *enterprise
        {
            return Ok(true);
        }
        Ok(user
            .team_id
            .as_ref()
            .context("Slack sender workspace is unavailable")?
            == &self.team
            && user.flags.is_stranger != Some(true))
    }

    pub async fn connect(
        service: &Arc<AdapterService>,
        deployment: &str,
        config: SlackAdapter,
        credentials: &BTreeMap<String, String>,
    ) -> Result<Arc<Self>> {
        initialize_tls()?;
        let client = Arc::new(SlackClient::new(SlackClientHyperConnector::new()?));
        Self::connect_with_client(service, deployment, config, credentials, client).await
    }
    pub(super) async fn connect_with_client(
        service: &Arc<AdapterService>,
        deployment: &str,
        config: SlackAdapter,
        credentials: &BTreeMap<String, String>,
        client: Arc<SlackHyperClient>,
    ) -> Result<Arc<Self>> {
        let token = SlackApiToken::new(
            credentials
                .get("bot-token")
                .context("Slack bot-token missing")?
                .clone()
                .into(),
        );
        let session = client.open_session(&token);
        let AuthResponse {
            auth,
            enterprise_id,
        } = session
            .http_session_api
            .http_get(
                "auth.test",
                &Vec::<(&str, Option<&str>)>::new(),
                Some(&slack_morphism::api::AUTH_TEST_SPECIAL_LIMIT_RATE_CTL),
            )
            .await?;
        let bot = auth
            .bot_id
            .as_ref()
            .context("Slack bot-token must identify a bot")?;
        let bot_info = session
            .bots_info(&SlackApiBotsInfoRequest::new().with_bot(bot.0.clone()))
            .await?;
        let connection = Arc::new(Self {
            account: format!("{}/{}", auth.team_id.0, auth.user_id.0),
            config: config.clone(),
            client: client.clone(),
            token,
            team: auth.team_id,
            enterprise: enterprise_id,
            user: auth.user_id,
            app: bot_info.bot.app_id,
            socket: Mutex::new(None),
        });
        if config.transport == SlackTransport::Socket {
            let environment = Arc::new(
                SlackClientEventsListenerEnvironment::new(client)
                    .with_user_state(SocketContext {
                        service: Arc::downgrade(service),
                        deployment: deployment.to_owned(),
                    })
                    .with_error_handler(socket_error),
            );
            let callbacks = SlackSocketModeListenerCallbacks::new()
                .with_push_events(socket_push)
                .with_interaction_events(socket_interaction);
            let socket = SlackClientSocketModeListener::new(
                &SlackClientSocketModeConfig::new().with_max_connections_count(1),
                environment,
                callbacks,
            );
            socket
                .listen_for(&SlackApiToken::new(
                    credentials
                        .get("app-token")
                        .context("Slack app-token missing")?
                        .clone()
                        .into(),
                ))
                .await?;
            socket.start().await;
            *connection.socket.lock().expect("Slack socket poisoned") = Some(socket);
        }
        tracing::info!(workspace = %connection.team.0, adapter = config.name.as_deref().unwrap_or("slack"), "Slack adapter connected");
        Ok(connection)
    }
    pub async fn shutdown(&self) {
        let socket = self.socket.lock().expect("Slack socket poisoned").take();
        if let Some(socket) = socket
            && tokio::time::timeout(std::time::Duration::from_secs(2), socket.shutdown())
                .await
                .is_err()
        {
            tracing::warn!("Slack socket shutdown timed out");
        }
    }
    pub async fn push(
        &self,
        service: &AdapterService,
        deployment: &Deployment,
        callback: SlackPushEventCallback,
    ) -> Result<()> {
        ensure!(
            callback.team_id == self.team && callback.api_app_id.0 == self.app,
            "Slack event belongs to another app or workspace"
        );
        let (sender, channel, ts, thread_ts, text, mention) = match callback.event {
            SlackEventCallbackBody::Message(message) => {
                if message.sender.bot_id.is_some()
                    || message.subtype.is_some()
                    || message.hidden == Some(true)
                {
                    return Ok(());
                }
                let Some(sender) = message.sender.user else {
                    return Ok(());
                };
                let Some(channel) = message.origin.channel else {
                    return Ok(());
                };
                let Some(text) = message.content.and_then(|content| content.text) else {
                    return Ok(());
                };
                (
                    sender,
                    channel,
                    message.origin.ts,
                    message.origin.thread_ts,
                    text,
                    false,
                )
            }
            SlackEventCallbackBody::AppMention(message) => {
                if message.edited.is_some() {
                    return Ok(());
                }
                let Some(text) = message.content.text else {
                    return Ok(());
                };
                (
                    message.user,
                    message.channel,
                    message.origin.ts,
                    message.origin.thread_ts,
                    text,
                    true,
                )
            }
            _ => return Ok(()),
        };
        if sender == self.user
            || text.trim().is_empty()
            || self
                .config
                .allowed_users
                .as_ref()
                .is_some_and(|users| !users.contains(&sender.0))
        {
            return Ok(());
        }
        let session = self.client.open_session(&self.token);
        let (direct, mut external) = if channel.0.starts_with('D') {
            (true, false)
        } else {
            let privacy: ChannelPrivacyResponse = session
                .http_session_api
                .http_get(
                    "conversations.info",
                    &vec![("channel", Some(channel.0.as_str()))],
                    None,
                )
                .await?;
            if privacy.channel.is_im == Some(true) || privacy.channel.is_mpim == Some(true) {
                (
                    true,
                    privacy
                        .channel
                        .is_ext_shared
                        .context("Slack DM shared status is unavailable")?,
                )
            } else {
                let shared = privacy
                    .channel
                    .is_ext_shared
                    .context("Slack channel shared status is unavailable")?;
                if shared && !self.config.allow_slack_connect {
                    return Ok(());
                }
                (false, shared)
            }
        };
        let sender_info = if direct {
            let info = session
                .users_info(&SlackApiUsersInfoRequest::new(sender.clone()))
                .await?;
            external |= !self.internal_user(&info.user)?;
            Some(info)
        } else {
            None
        };
        if external
            && !(self.config.allow_slack_connect
                && self
                    .config
                    .allowed_channels
                    .as_ref()
                    .is_some_and(|channels| channels.contains(&channel.0)))
        {
            return Ok(());
        }
        if direct {
            if !self.config.direct_messages {
                return Ok(());
            }
        } else if self
            .config
            .allowed_channels
            .as_ref()
            .is_some_and(|channels| !channels.contains(&channel.0))
        {
            return Ok(());
        }
        let root = thread_ts.as_ref().unwrap_or(&ts).0.clone();
        let conversation = if direct && thread_ts.is_none() {
            format!("{}/{}", self.team.0, channel.0)
        } else {
            format!("{}/{}/{root}", self.team.0, channel.0)
        };
        let input = IncomingMessage {
            id: format!("{}/{}/{}", self.team.0, channel.0, ts.0),
            account: self.account.clone(),
            conversation,
            sender: sender.0.clone(),
            text,
            channel: channel.0,
            message_ts: ts.0,
            reply_ts: if direct && thread_ts.is_none() {
                None
            } else {
                Some(root)
            },
            direct,
            external,
        };
        let tracked = service
            .store
            .get::<Route>(&AdapterStore::route(&deployment.id, &input))
            .await?
            .is_some();
        if !direct
            && self.config.trigger == ChannelTrigger::MentionsOnly
            && !mention
            && !input.text.contains(&format!("<@{}>", self.user.0))
            && !tracked
        {
            return Ok(());
        }
        let grant = if self.config.use_sender_vault {
            let mut email = None;
            if service
                .identities
                .needs_slack_email(&self.team.0, &sender.0)
                .await?
            {
                let info = match sender_info {
                    Some(info) => Ok(info),
                    None => {
                        session
                            .users_info(&SlackApiUsersInfoRequest::new(sender.clone()))
                            .await
                    }
                };
                match info {
                    Ok(info)
                        if self.internal_user(&info.user)?
                            && info.user.flags.is_bot != Some(true)
                            && info.user.flags.is_restricted != Some(true)
                            && info.user.flags.is_ultra_restricted != Some(true) =>
                    {
                        email = info
                            .user
                            .profile
                            .and_then(|profile| profile.email)
                            .map(|email| email.0);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error, "Slack directory lookup unavailable; using shared vault")
                    }
                }
            }
            service
                .identities
                .resolve_slack(&self.team.0, &sender.0, email.as_deref())
                .await?
        } else {
            None
        };
        service.accept(deployment, input, grant).await
    }
    pub async fn interaction(
        &self,
        service: &AdapterService,
        deployment: &Deployment,
        event: SlackInteractionEvent,
    ) -> Result<()> {
        let SlackInteractionEvent::BlockActions(event) = event else {
            return Ok(());
        };
        ensure!(
            event.team.id == self.team && event.api_app_id.0 == self.app,
            "Slack action belongs to another app or workspace"
        );
        let user = event.user.context("Slack action has no user")?;
        let channel = event.channel.context("Slack action has no channel")?;
        let SlackInteractionActionContainer::Message(container) = event.container else {
            anyhow::bail!("approval is not a message action");
        };
        for action in event.actions.unwrap_or_default() {
            let approved = match action.action_id.0.as_str() {
                "exo_approve" => true,
                "exo_deny" => false,
                _ => continue,
            };
            service
                .accept_decision(
                    deployment,
                    &action.value.context("approval action has no value")?,
                    &user.id.0,
                    &channel.id.0,
                    &container.message_ts.0,
                    approved,
                )
                .await?;
        }
        Ok(())
    }
    pub(super) async fn send(&self, message: &Outbox) -> Result<Option<String>> {
        let session = self.client.open_session(&self.token);
        let content = match &message.kind {
            DeliveryKind::Stream {
                chunks,
                complete,
                sender,
                thread_ts,
            } => {
                let batch = progress::batch(chunks).to_vec();
                let response = match &message.external_id {
                    None => {
                        session
                            .chat_start_stream(
                                &SlackApiChatStartStreamRequest::new(
                                    message.channel.clone().into(),
                                )
                                .with_thread_ts(thread_ts.clone().into())
                                .with_recipient_user_id(sender.clone().into())
                                .with_recipient_team_id(self.team.clone())
                                .with_task_display_mode(SlackStreamTaskDisplayMode::Timeline)
                                .with_chunks(batch),
                            )
                            .await?
                            .ts
                    }
                    Some(ts) if *complete && batch.len() == chunks.len() => {
                        let mut request = SlackApiChatStopStreamRequest::new(
                            message.channel.clone().into(),
                            ts.clone().into(),
                            SlackMessageContent::new(),
                        );
                        if !batch.is_empty() {
                            request.chunks = Some(batch);
                        }
                        session.chat_stop_stream(&request).await?.ts
                    }
                    Some(ts) => {
                        session
                            .chat_append_stream(
                                &SlackApiChatAppendStreamRequest::new(
                                    message.channel.clone().into(),
                                    ts.clone().into(),
                                )
                                .with_chunks(batch),
                            )
                            .await?
                            .ts
                    }
                };
                return Ok(Some(response.0));
            }
            DeliveryKind::Status {
                thread_ts, text, ..
            } => {
                let request = ThreadStatusRequest {
                    status: SlackApiAssistantThreadsSetStatusRequest::new(
                        message.channel.clone().into(),
                        if text.is_empty() {
                            String::new()
                        } else {
                            "is working…".into()
                        },
                        thread_ts.clone().into(),
                    ),
                    loading_messages: if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![text.clone()]
                    },
                };
                let _: SlackApiAssistantThreadsSetStatusResponse = session
                    .http_session_api
                    .http_post(
                        "assistant.threads.setStatus",
                        &request,
                        Some(&ASSISTANT_THREAD_SET_STATUS_SPECIAL_LIMIT_RATE_CTL),
                    )
                    .await?;
                return Ok(None);
            }
            DeliveryKind::Reaction { message_ts } => {
                session
                    .reactions_add(&SlackApiReactionsAddRequest::new(
                        message.channel.clone().into(),
                        "eyes".into(),
                        message_ts.clone().into(),
                    ))
                    .await?;
                return Ok(None);
            }
            DeliveryKind::Text { text } => SlackMessageContent::new().with_text(text.clone()),
            DeliveryKind::Approval { approval, .. } => {
                let arguments = serde_json::to_string(&approval.request.arguments)?;
                let description = format!(
                    "Approval required for {}:\n{}",
                    approval.request.function_name,
                    arguments.chars().take(2000).collect::<String>()
                );
                SlackMessageContent::new()
                    .with_text(description.clone())
                    .with_blocks(slack_blocks![
                        SlackSectionBlock::new().with_text(md!(description)),
                        SlackActionsBlock::new(slack_blocks![
                            SlackBlockButtonElement::new(pt!("Approve"))
                                .with_action_id("exo_approve".into())
                                .with_value(message.id.clone())
                                .with_style(SlackBlockButtonStyle::Primary),
                            SlackBlockButtonElement::new(pt!("Deny"))
                                .with_action_id("exo_deny".into())
                                .with_value(message.id.clone())
                                .with_style(SlackBlockButtonStyle::Danger),
                        ])
                    ])
            }
        };
        let mut request =
            SlackApiChatPostMessageRequest::new(message.channel.clone().into(), content)
                .with_unfurl_links(false)
                .with_unfurl_media(false);
        if let Some(ts) = &message.reply_ts {
            request = request.with_thread_ts(ts.clone().into());
        }
        Ok(Some(session.chat_post_message(&request).await?.ts.0))
    }
}

async fn socket_context(
    states: SlackClientEventsUserState,
) -> Result<(Arc<AdapterService>, Deployment)> {
    let context = states
        .read()
        .await
        .get_user_state::<SocketContext>()
        .cloned()
        .context("Slack socket state missing")?;
    let service = context
        .service
        .upgrade()
        .context("adapter service stopped")?;
    let deployment = service
        .deployment(&context.deployment)
        .context("Slack deployment is reconnecting")?;
    Ok((service, deployment))
}
async fn socket_push(
    event: SlackPushEventCallback,
    _: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (service, deployment) = socket_context(states).await?;
    let connection = &deployment.connection;
    connection.push(&service, &deployment, event).await?;
    Ok(())
}
async fn socket_interaction(
    event: SlackInteractionEvent,
    _: Arc<SlackHyperClient>,
    states: SlackClientEventsUserState,
) -> std::result::Result<SlackInteractionResponse, Box<dyn std::error::Error + Send + Sync>> {
    let (service, deployment) = socket_context(states).await?;
    let connection = &deployment.connection;
    connection.interaction(&service, &deployment, event).await?;
    Ok(SlackInteractionResponse::Empty)
}
fn socket_error(
    error: Box<dyn std::error::Error + Send + Sync>,
    _: Arc<SlackHyperClient>,
    _: SlackClientEventsUserState,
) -> HttpStatusCode {
    tracing::error!(%error, "Slack socket event was not accepted");
    HttpStatusCode::INTERNAL_SERVER_ERROR
}
