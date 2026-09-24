use std::{collections::HashMap, sync::Arc};

use futures::stream::BoxStream;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::{
        auth::AuthError,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};

use crate::{McpAuthenticationError, McpCredentialProvider, McpServerConfig};

type TransportError = StreamableHttpError<ClientError>;
type HttpError = StreamableHttpError<reqwest::Error>;

#[derive(Debug)]
pub(super) struct ClientError {
    error: anyhow::Error,
    authentication: Option<McpAuthenticationError>,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
type EventStream = BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>;

#[derive(Clone)]
pub(super) struct CredentialClient {
    client: reqwest::Client,
    server: McpServerConfig,
    credentials: Arc<dyn McpCredentialProvider>,
}

impl CredentialClient {
    pub(super) fn new(
        client: reqwest::Client,
        server: McpServerConfig,
        credentials: Arc<dyn McpCredentialProvider>,
    ) -> Self {
        Self {
            client,
            server,
            credentials,
        }
    }

    async fn send<T, F, Fut>(&self, call: F) -> Result<T, TransportError>
    where
        F: Fn(Option<String>) -> Fut,
        Fut: Future<Output = Result<T, HttpError>>,
    {
        let credential = self
            .credentials
            .resolve(&self.server)
            .await
            .map_err(credential_error)?;
        let supplied = credential.is_some();
        let result = match call(credential.as_ref().map(|c| c.token.clone())).await {
            Err(HttpError::AuthRequired(challenge)) => {
                if let Some(rejected) = credential
                    && let Some(fresh) = self
                        .credentials
                        .refresh(&self.server, &rejected)
                        .await
                        .map_err(credential_error)?
                    && fresh.token != rejected.token
                {
                    call(Some(fresh.token)).await
                } else {
                    Err(HttpError::AuthRequired(challenge))
                }
            }
            result => result,
        };
        result.map_err(|error| {
            if error.auth_challenge().is_some() {
                TransportError::Client(ClientError {
                    error: anyhow::Error::new(error),
                    authentication: Some(McpAuthenticationError {
                        server_name: self.server.name.clone(),
                        credential_supplied: supplied,
                        details: self
                            .credentials
                            .authentication_failure_context(&self.server, supplied),
                    }),
                })
            } else {
                map_transport_error(error)
            }
        })
    }
}

fn credential_error(error: anyhow::Error) -> TransportError {
    AuthError::CredentialStoreError(format!("{error:#}")).into()
}

impl StreamableHttpClient for CredentialClient {
    type Error = ClientError;

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), TransportError> {
        self.send(|token| {
            self.client.delete_session(
                uri.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
            )
        })
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<EventStream, TransportError> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            StreamableHttpClientTransportConfig::default().max_sse_event_size,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<EventStream, TransportError> {
        self.send(|token| {
            self.client.get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                token,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, TransportError> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            StreamableHttpClientTransportConfig::default().max_sse_event_size,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, TransportError> {
        self.send(|token| {
            self.client.post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }
}

pub(super) fn authentication_context(error: impl Into<anyhow::Error>) -> anyhow::Error {
    let error = error.into();
    let transport = match error.downcast_ref::<rmcp::service::ClientInitializeError>() {
        Some(rmcp::service::ClientInitializeError::TransportError { error, .. }) => Some(error),
        _ => match error.downcast_ref::<rmcp::service::ServiceError>() {
            Some(rmcp::service::ServiceError::TransportSend(error)) => Some(error),
            _ => None,
        },
    };
    let authentication = transport.and_then(|transport| {
        let TransportError::Client(client) = transport.error.downcast_ref::<TransportError>()?
        else {
            return None;
        };
        client.authentication.clone()
    });
    match authentication {
        Some(authentication) => error.context(authentication),
        None => error,
    }
}

fn map_transport_error(error: HttpError) -> TransportError {
    match error {
        HttpError::Sse(error) => TransportError::Sse(error),
        HttpError::Io(error) => TransportError::Io(error),
        HttpError::UnexpectedEndOfStream => TransportError::UnexpectedEndOfStream,
        HttpError::UnexpectedServerResponse(response) => {
            TransportError::UnexpectedServerResponse(response)
        }
        HttpError::UnexpectedContentType(content_type) => {
            TransportError::UnexpectedContentType(content_type)
        }
        HttpError::ServerDoesNotSupportSse => TransportError::ServerDoesNotSupportSse,
        HttpError::ServerDoesNotSupportDeleteSession => {
            TransportError::ServerDoesNotSupportDeleteSession
        }
        HttpError::TokioJoinError(error) => TransportError::TokioJoinError(error),
        HttpError::Deserialize(error) => TransportError::Deserialize(error),
        HttpError::TransportChannelClosed => TransportError::TransportChannelClosed,
        HttpError::MissingSessionIdInResponse => TransportError::MissingSessionIdInResponse,
        HttpError::Auth(error) => TransportError::Auth(error),
        HttpError::AuthRequired(error) => TransportError::AuthRequired(error),
        HttpError::InsufficientScope(error) => TransportError::InsufficientScope(error),
        HttpError::ReservedHeaderConflict(header) => TransportError::ReservedHeaderConflict(header),
        HttpError::SessionExpired => TransportError::SessionExpired,
        HttpError::SessionRecoveryTimeout => TransportError::SessionRecoveryTimeout,
        HttpError::ControlRequestTimeout => TransportError::ControlRequestTimeout,
        HttpError::Client(error) => TransportError::Client(ClientError {
            error: error.into(),
            authentication: None,
        }),
        error => TransportError::Client(ClientError {
            error: error.into(),
            authentication: None,
        }),
    }
}
