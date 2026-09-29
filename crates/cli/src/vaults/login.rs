use super::*;
use clap::ValueEnum;
use exoharness::vault::{OAuthRefresh, github_cli_account, github_cli_command};
use oauth2::{
    AuthType, ClientId, ClientSecret, DeviceAuthorizationUrl, Scope,
    StandardDeviceAuthorizationResponse, TokenResponse, TokenUrl, basic::BasicClient,
};
use rmcp::transport::auth::AuthorizationManager;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Preset {
    Github,
    Openai,
}

#[derive(Debug, Args)]
#[group(skip)]
#[command(next_help_heading = "Credential source")]
#[command(group(clap::ArgGroup::new("login_source").multiple(true).args(["preset", "url", "client_id"])))]
pub struct CredentialArgs {
    /// Read an existing token from this environment variable instead of logging in.
    #[arg(long, value_parser = crate::parse_env_var_name,
        conflicts_with_all = ["url", "client_id", "scope", "device_url", "token_url", "no_browser"])]
    token_env: Option<String>,
    /// Supply login, credential policy, and secret-name defaults.
    #[arg(long, conflicts_with = "url")]
    preset: Option<Preset>,
    /// Discover OAuth from an MCP resource URL.
    #[arg(long)]
    url: Option<String>,
    /// OAuth client ID; skips registration (and uses OAuth directly for GitHub).
    #[arg(long)]
    client_id: Option<String>,
    /// Read an OAuth client secret from this environment variable.
    #[arg(long, requires = "client_id", value_parser = crate::parse_env_var_name)]
    client_secret_env: Option<String>,
    /// OAuth scopes to request; with GitHub CLI, adds to its existing permissions. Repeat as needed.
    #[arg(long, requires = "login_source")]
    scope: Vec<String>,
    /// OAuth device authorization endpoint; requires --token-url and --client-id.
    #[arg(long, requires_all = ["token_url", "client_id"], conflicts_with = "url")]
    device_url: Option<String>,
    /// OAuth token endpoint for device login.
    #[arg(long, requires = "client_id", conflicts_with = "url")]
    token_url: Option<String>,
    /// Print login instructions without opening a browser.
    #[arg(long, requires = "login_source")]
    no_browser: bool,
}

impl CredentialArgs {
    pub(super) fn default_name(&self) -> Option<&'static str> {
        match self.preset {
            Some(Preset::Github) => Some("github"),
            Some(Preset::Openai) => Some("openai"),
            None => None,
        }
    }

    pub(super) fn default_policy(&self) -> Result<Option<CredentialPolicy>> {
        match self.preset {
            Some(Preset::Github) => Ok(Some(CredentialPolicy::destinations(vec![
                CredentialDestination::origin("https://github.com")?,
                CredentialDestination::origin("https://api.github.com")?,
            ]))),
            Some(Preset::Openai) => Ok(Some(
                CredentialDestination::origin("https://api.openai.com")?.into(),
            )),
            None => self
                .url
                .as_deref()
                .map(CredentialDestination::url)
                .transpose()
                .map(|destination| destination.map(Into::into)),
        }
    }

    pub(super) async fn read(
        &self,
        env: &HashMap<String, String>,
        policy: Option<&CredentialPolicy>,
        local: bool,
    ) -> Result<Option<Secret>> {
        if let Some(variable) = &self.token_env {
            return Ok(Some(Secret::Key {
                value: crate::env_value_from_arg("--token-env", variable, env)?,
            }));
        }
        if let Some(Preset::Openai) = self.preset {
            ensure!(
                self.client_id.is_none()
                    && self.client_secret_env.is_none()
                    && self.scope.is_empty()
                    && self.device_url.is_none()
                    && self.token_url.is_none()
                    && !self.no_browser,
                "--preset openai reads OPENAI_API_KEY and does not use OAuth options"
            );
            return Ok(Some(Secret::Key {
                value: crate::env_value_from_arg("--preset openai", "OPENAI_API_KEY", env)
                    .context("set OPENAI_API_KEY for --preset openai")?,
            }));
        }
        if self.preset.is_none() && self.url.is_none() && self.client_id.is_none() {
            return Ok(None);
        }
        ensure!(
            policy.is_some(),
            "provide a credential policy with --allow-origin, --allow-url, or --policy"
        );
        let client_secret = self
            .client_secret_env
            .as_deref()
            .map(|variable| crate::env_value_from_arg("--client-secret-env", variable, env))
            .transpose()?;
        let secret = if let Some(resource) = &self.url {
            discover(self, resource, client_secret).await?
        } else if self.client_id.is_some() {
            device(self, client_secret).await?
        } else {
            github_cli(self, local).await?
        };
        exoharness::vault::validate_secret(&secret, policy)?;
        Ok(Some(secret))
    }
}

async fn discover(
    args: &CredentialArgs,
    resource: &str,
    client_secret: Option<String>,
) -> Result<Secret> {
    CredentialDestination::url(resource)?;
    let mut manager = AuthorizationManager::new(resource).await?;
    let challenge = exo_mcp::probe_auth_challenge(resource).await?;
    let resolution = manager
        .resolve_metadata_from_challenge(challenge.as_deref())
        .await?;
    ensure!(
        resolution.source.is_discovered(),
        "resource does not advertise OAuth; import a token with `exo vault secret create --token-env`"
    );
    #[derive(serde::Deserialize)]
    struct Authentication {
        #[serde(default)]
        token_endpoint_auth_methods_supported: Vec<String>,
    }
    let authentication: Authentication = serde_json::from_value(serde_json::to_value(
        &resolution.metadata.additional_fields,
    )?)?;
    let methods = authentication.token_endpoint_auth_methods_supported;
    let client_secret_basic = client_secret.is_some()
        && (methods.iter().any(|m| m == "client_secret_basic")
            || !methods.iter().any(|m| m == "client_secret_post"));
    let token_endpoint = resolution.metadata.token_endpoint.clone();
    manager.set_metadata(resolution.metadata);
    let grant = crate::oauth::authorize_with(
        manager,
        &args.scope,
        args.client_id.as_deref(),
        client_secret.as_deref(),
        |url| crate::oauth::open_browser(url, args.no_browser),
        Duration::from_secs(300),
    )
    .await?;
    let response = grant
        .credentials
        .token_response
        .context("OAuth token missing")?;
    let received_at = grant
        .credentials
        .token_received_at
        .context("OAuth receipt time missing")?;
    Ok(oauth_secret(
        response,
        received_at,
        OAuthRefresh {
            token_endpoint,
            client_id: grant.credentials.client_id,
            client_secret,
            client_secret_basic,
            resource: grant.resource,
            scopes: grant.credentials.granted_scopes,
        },
    ))
}

async fn device(args: &CredentialArgs, client_secret: Option<String>) -> Result<Secret> {
    let device_url = args
        .device_url
        .as_deref()
        .or(args.preset.map(|_| "https://github.com/login/device/code"))
        .context("provide --device-url")?;
    let token_url = args
        .token_url
        .as_deref()
        .or(args
            .preset
            .map(|_| "https://github.com/login/oauth/access_token"))
        .context("provide --token-url")?;
    for endpoint in [device_url, token_url] {
        exoharness::vault::validate_oauth_endpoint(endpoint)?;
    }
    let client_id = args.client_id.as_ref().context("provide --client-id")?;
    let mut client = BasicClient::new(ClientId::new(client_id.clone()))
        .set_auth_type(AuthType::RequestBody)
        .set_device_authorization_url(DeviceAuthorizationUrl::new(device_url.into())?)
        .set_token_uri(TokenUrl::new(token_url.into())?);
    if let Some(secret) = &client_secret {
        client = client.set_client_secret(ClientSecret::new(secret.clone()));
    }
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let details: StandardDeviceAuthorizationResponse = client
        .exchange_device_code()
        .add_scopes(args.scope.iter().cloned().map(Scope::new))
        .request_async(&http)
        .await
        .map_err(device_error)
        .context(
            "could not start OAuth device login; check the client ID and device-flow configuration",
        )?;
    exoharness::vault::validate_oauth_endpoint(details.verification_uri().as_str())?;
    println!("Enter code: {}", details.user_code().secret());
    crate::oauth::open_browser(details.verification_uri().to_string(), args.no_browser).await?;
    let response = tokio::select! {
        response = client.exchange_device_access_token(&details).request_async(&http, tokio::time::sleep, Some(Duration::from_secs(900))) =>
            response.map_err(device_error).context("OAuth device login failed; run the login command again")?,
        result = tokio::signal::ctrl_c() => { result?; bail!("OAuth login canceled"); }
    };
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    Ok(oauth_secret(
        response,
        now,
        OAuthRefresh {
            token_endpoint: token_url.into(),
            client_id: client_id.clone(),
            client_secret,
            client_secret_basic: false,
            resource: None,
            scopes: args.scope.clone(),
        },
    ))
}

fn oauth_secret(
    response: impl TokenResponse,
    received_at: u64,
    mut refresh: OAuthRefresh,
) -> Secret {
    if let Some(scopes) = response.scopes() {
        refresh.scopes = scopes
            .iter()
            .map(|scope| scope.as_str().to_owned())
            .collect();
    }
    Secret::Oauth {
        access_token: response.access_token().secret().clone(),
        refresh_token: response.refresh_token().map(|token| token.secret().clone()),
        expires_at: response
            .expires_in()
            .map(|ttl| received_at.saturating_add(ttl.as_secs())),
        refresh: Some(refresh),
    }
}

async fn github_cli(args: &CredentialArgs, local: bool) -> Result<Secret> {
    let mut login = github_cli_account().await?;
    if login.is_none() || !args.scope.is_empty() {
        let mut auth = github_cli_command();
        auth.args([
            "auth",
            "login",
            "--web",
            "--hostname",
            "github.com",
            "--git-protocol",
            "https",
        ]);
        for scope in &args.scope {
            auth.args(["--scopes", scope]);
        }
        if args.no_browser {
            auth.env("GH_BROWSER", "true");
        }
        ensure!(auth.status().await?.success(), "GitHub login failed");
        login = github_cli_account().await?;
    }
    let account = login.context("GitHub CLI login did not produce an authenticated account")?;
    let value = exoharness::vault::github_cli_token(&account).await?;
    if local {
        eprintln!(
            "Linked GitHub CLI account {account}; token changes in `gh` are picked up automatically."
        );
        Ok(Secret::GithubCli { value, account })
    } else {
        eprintln!(
            "Copied the GitHub CLI token to the remote vault. Use `exo vault secret update <vault> <secret> --preset github` after changing it in `gh`."
        );
        Ok(Secret::Key { value })
    }
}

fn device_error<RE, T>(
    error: oauth2::RequestTokenError<RE, oauth2::StandardErrorResponse<T>>,
) -> anyhow::Error
where
    RE: std::error::Error + Send + Sync + 'static,
    T: oauth2::ErrorResponseType + std::fmt::Display + 'static,
{
    match error {
        oauth2::RequestTokenError::ServerResponse(response) => anyhow::anyhow!(
            "OAuth server rejected the request ({})",
            response.error().to_string().escape_default()
        ),
        oauth2::RequestTokenError::Request(error) => anyhow::Error::new(error),
        oauth2::RequestTokenError::Parse(..) => {
            anyhow::anyhow!("OAuth server returned an invalid response")
        }
        oauth2::RequestTokenError::Other(message) => {
            anyhow::anyhow!("{}", message.escape_default())
        }
    }
}
