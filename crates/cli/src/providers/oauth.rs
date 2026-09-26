use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use exo_managed_agents::http::RuntimeClient;
use exoharness::AccessTokenProvider;
use oauth2::TokenResponse;
use rmcp::transport::auth::{AuthError, AuthorizationManager, CredentialStore, StoredCredentials};
use serde::{Deserialize, Serialize};

use super::{Connection, Profile};

#[cfg(test)]
mod tests;

const SERVICE: &str = "exo-provider";

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Credential {
    OAuth { credentials: StoredCredentials },
}

#[derive(Clone)]
struct KeychainStore {
    entry: Arc<keyring_core::Entry>,
    lock_path: PathBuf,
}

impl KeychainStore {
    fn new(profile: &Profile, directory: &Path) -> Result<Self> {
        static INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();
        let initialized =
            INIT.get_or_init(|| keyring::use_native_store(true).map_err(|error| error.to_string()));
        initialized.as_ref().map_err(|error| anyhow!(
            "provider credential store unavailable: {error}; use exo provider login <name> --api-key-env <ENV_VAR> for headless access"
        ))?;
        Ok(Self {
            entry: Arc::new(keyring_core::Entry::new(SERVICE, &profile.id.to_string())?),
            lock_path: directory.join("auth").join(format!("{}.lock", profile.id)),
        })
    }

    fn read(&self) -> Result<Option<StoredCredentials>> {
        match self.entry.get_password() {
            Ok(text) => {
                let Credential::OAuth { credentials } = serde_json::from_str(&text)?;
                Ok(Some(credentials))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn write(&self, credentials: StoredCredentials) -> Result<()> {
        self.entry
            .set_password(&serde_json::to_string(&Credential::OAuth { credentials })?)?;
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        match self.entry.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn lock(&self) -> Result<std::fs::File> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let directory = self
            .lock_path
            .parent()
            .context("credential lock directory missing")?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&self.lock_path)?;
        tokio::task::spawn_blocking(move || {
            file.lock()?;
            Ok(file)
        })
        .await?
    }

    async fn logout(&self) -> Result<()> {
        let _guard = self.lock().await?;
        self.delete()
    }
}

fn store_error(error: anyhow::Error) -> AuthError {
    AuthError::CredentialStoreError(error.to_string())
}

#[async_trait]
impl CredentialStore for KeychainStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        self.read().map_err(store_error)
    }

    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        self.write(credentials).map_err(store_error)
    }

    async fn clear(&self) -> std::result::Result<(), AuthError> {
        self.delete().map_err(store_error)
    }
}

pub(super) async fn clear(profile: &Profile, directory: &Path) -> Result<()> {
    if !profile.stored_credentials {
        return Ok(());
    }
    KeychainStore::new(profile, directory)?.logout().await
}

async fn manager(profile: &Profile) -> Result<AuthorizationManager> {
    let Connection::Http { endpoint, .. } = &profile.connection else {
        bail!("local providers do not require login");
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut manager = AuthorizationManager::new(endpoint.as_str()).await?;
    let identity_url = profile.client()?.identity_url()?;
    let response = client
        .get(identity_url.clone())
        .send()
        .await
        .with_context(|| format!("connecting to provider at {identity_url}; check its --url"))?;
    if !response.status().is_success() && response.status() != reqwest::StatusCode::UNAUTHORIZED {
        bail!(
            "provider OAuth discovery at {identity_url} failed ({}); check the provider's --url",
            response.status()
        );
    }
    let challenge = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .map(|value| value.to_str())
        .transpose()?;
    let resolution = manager.resolve_metadata_from_challenge(challenge).await?;
    if !resolution.source.is_discovered() {
        bail!(
            "provider at {identity_url} does not advertise OAuth metadata; configure an API key instead"
        );
    }
    manager.set_metadata(resolution.metadata);
    Ok(manager)
}

struct TokenState {
    manager: Option<AuthorizationManager>,
    token: Option<String>,
    account: Option<String>,
}

struct ProviderTokens {
    profile: Profile,
    store: KeychainStore,
    state: tokio::sync::Mutex<TokenState>,
}

impl ProviderTokens {
    fn new(profile: &Profile, store: KeychainStore) -> Self {
        Self {
            profile: profile.clone(),
            store,
            state: tokio::sync::Mutex::new(TokenState {
                manager: None,
                token: None,
                account: profile.account_id.clone(),
            }),
        }
    }
}

#[async_trait]
impl AccessTokenProvider for ProviderTokens {
    async fn access_token(&self) -> Result<String> {
        let _guard = self.store.lock().await?;
        let mut state = self.state.lock().await;
        let token = match self.store.read()? {
            Some(_) => {
                if state.manager.is_none() {
                    let mut manager = manager(&self.profile).await?;
                    manager.set_credential_store(self.store.clone());
                    state.manager = Some(manager);
                }
                let manager = state.manager.as_mut().context("OAuth manager missing")?;
                if !manager.initialize_from_store().await? {
                    bail!("provider login is required; run exo provider login");
                }
                manager
                    .get_access_token()
                    .await
                    .map_err(|error| match error {
                        AuthError::AuthorizationRequired => {
                            anyhow!("provider login expired; run exo provider login")
                        }
                        other => other.into(),
                    })?
            }
            None => bail!(
                "provider is not logged in; run exo provider login or configure --api-key-env"
            ),
        };
        if token.trim().is_empty() {
            bail!("provider credential is empty");
        }
        if state.token.as_ref() != Some(&token) {
            let account = self
                .profile
                .client()?
                .with_bearer_token(token.clone())
                .identity()
                .await?
                .account_id;
            if state
                .account
                .as_ref()
                .is_some_and(|saved| saved != &account)
            {
                bail!(
                    "provider is authenticated as a different account; run exo provider login to switch explicitly"
                );
            }
            state.account = Some(account);
            state.token = Some(token.clone());
        }
        Ok(token)
    }
}

pub(super) async fn client(profile: &Profile, directory: &Path) -> Result<(RuntimeClient, String)> {
    let tokens = Arc::new(ProviderTokens::new(
        profile,
        KeychainStore::new(profile, directory)?,
    ));
    tokens.access_token().await?;
    let account = tokens
        .state
        .lock()
        .await
        .account
        .clone()
        .context("provider identity missing")?;
    Ok((profile.client()?.with_token_provider(tokens), account))
}

async fn login_with<F, Fut>(
    profile: &Profile,
    store: &KeychainStore,
    launch: F,
    timeout: Duration,
) -> Result<String>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let previous = {
        let _guard = store.lock().await?;
        serde_json::to_vec(&store.read()?)?
    };
    let grant = crate::oauth::authorize_with(
        manager(profile).await?,
        &profile.scopes,
        profile.client_id.as_deref(),
        launch,
        timeout,
    )
    .await?;
    let token = grant
        .credentials
        .token_response
        .as_ref()
        .context("OAuth token missing")?
        .access_token()
        .secret()
        .clone();
    let account = profile
        .client()?
        .with_bearer_token(token)
        .identity()
        .await?
        .account_id;
    let _guard = store.lock().await?;
    if serde_json::to_vec(&store.read()?)? != previous {
        bail!("provider credentials changed during login; retry the login command");
    }
    store.save(grant.credentials).await?;
    Ok(account)
}

pub(super) async fn login(profile: &Profile, no_browser: bool, directory: &Path) -> Result<String> {
    let store = KeychainStore::new(profile, directory)?;
    login_with(
        profile,
        &store,
        |url| crate::oauth::open_browser(url, no_browser),
        Duration::from_secs(300),
    )
    .await
}
