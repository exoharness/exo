use super::OAuthHttpClient;
use crate::Secret;
use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use oauth2::{
    AuthType, ClientId, ClientSecret, RefreshToken, Scope, TokenResponse, TokenUrl,
    basic::BasicClient,
};
#[cfg(feature = "basic-backend")]
use std::time::Duration;

fn now() -> u64 {
    chrono::Utc::now()
        .timestamp()
        .try_into()
        .expect("clock before Unix epoch")
}

pub(super) fn needs_refresh(secret: &Secret) -> bool {
    matches!(secret, Secret::Oauth { expires_at: Some(expires_at), .. } if *expires_at <= now().saturating_add(60))
}

pub(super) async fn refresh(secret: Secret, http: &dyn OAuthHttpClient) -> Result<Secret> {
    let Secret::Oauth {
        refresh_token,
        refresh,
        ..
    } = secret
    else {
        bail!("credential is not an OAuth grant");
    };
    let mut config =
        refresh.context("OAuth refresh credentials are missing; re-authorize this credential")?;
    let refresh_token = RefreshToken::new(
        refresh_token
            .context("OAuth refresh credentials are missing; re-authorize this credential")?,
    );
    let mut client = BasicClient::new(ClientId::new(config.client_id.clone()))
        .set_auth_type(if config.client_secret_basic {
            AuthType::BasicAuth
        } else {
            AuthType::RequestBody
        })
        .set_token_uri(TokenUrl::new(config.token_endpoint.clone())?);
    if let Some(secret) = &config.client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.clone()));
    }
    let mut request = client
        .exchange_refresh_token(&refresh_token)
        .add_scopes(config.scopes.iter().cloned().map(Scope::new));
    if let Some(resource) = &config.resource {
        request = request.add_extra_param("resource", resource);
    }
    let received_at = now();
    let response =
        request
            .request_async(&HttpAdapter(http))
            .await
            .map_err(|error| match error {
                oauth2::RequestTokenError::ServerResponse(response) => {
                    if matches!(
                        response.error(),
                        oauth2::basic::BasicErrorResponseType::InvalidGrant
                            | oauth2::basic::BasicErrorResponseType::InvalidClient
                    ) {
                        anyhow::anyhow!(
                            "OAuth refresh rejected ({}); re-authorize this credential",
                            response.error()
                        )
                    } else {
                        anyhow::anyhow!("OAuth token refresh failed ({})", response.error())
                    }
                }
                oauth2::RequestTokenError::Request(error) => {
                    anyhow::Error::new(error).context("OAuth token refresh failed")
                }
                other => anyhow::anyhow!("OAuth token refresh failed: {other}"),
            })?;
    let refresh_token = response
        .refresh_token()
        .unwrap_or(&refresh_token)
        .secret()
        .clone();
    let expires_at = response
        .expires_in()
        .map(|ttl| received_at.saturating_add(ttl.as_secs()));
    if let Some(scopes) = response.scopes() {
        config.scopes = scopes
            .iter()
            .map(|scope| scope.as_str().to_owned())
            .collect();
    }
    Ok(Secret::Oauth {
        access_token: response.access_token().secret().clone(),
        refresh_token: Some(refresh_token),
        expires_at,
        refresh: Some(config),
    })
}

struct HttpAdapter<'a>(&'a dyn OAuthHttpClient);
impl<'c> oauth2::AsyncHttpClient<'c> for HttpAdapter<'_> {
    type Error = std::io::Error;
    type Future = BoxFuture<'c, Result<oauth2::HttpResponse, Self::Error>>;
    fn call(&'c self, request: oauth2::HttpRequest) -> Self::Future {
        Box::pin(async move { self.0.request(request).await.map_err(std::io::Error::other) })
    }
}

#[cfg(feature = "basic-backend")]
pub(super) struct NativeOAuthHttpClient;
#[cfg(feature = "basic-backend")]
#[async_trait::async_trait]
impl OAuthHttpClient for NativeOAuthHttpClient {
    async fn request(&self, request: oauth2::HttpRequest) -> Result<oauth2::HttpResponse> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(oauth2::AsyncHttpClient::call(&client, request).await?)
    }
}
