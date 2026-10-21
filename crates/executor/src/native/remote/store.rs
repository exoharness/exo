use super::oidc::{AuthConfig, Identity};
use anyhow::{Context, Result, ensure};
use exoharness::{
    ExoHarness, PutSecretRequest, Secret, SecretId, Uuid7,
    vault::{VaultHandle, VaultId},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::Mutex;

pub(super) const STATE_SECRET: &str = "exo-server-state";
const STORE_LOCATION: &str = "identity/store.json";

#[derive(Serialize, Deserialize)]
struct StoreLocation {
    vault: VaultId,
    secret: SecretId,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct State {
    pub deployment: String,
    pub owner: String,
    pub owners: BTreeMap<String, String>,
    pub vault_names: BTreeMap<VaultId, String>,
    pub vault_owners: BTreeMap<VaultId, String>,
    pub shared_vaults: Vec<VaultId>,
    pub principals: BTreeMap<String, Principal>,
    pub clients: BTreeMap<String, Client>,
    pub sessions: BTreeMap<String, Session>,
    #[serde(default)]
    pub slack_pins: Vec<exo_managed_agents::identity::SlackIdentityPin>,
    #[serde(default)]
    pub trusted_slack_workspaces: Vec<String>,
    #[serde(default)]
    pub identity_revision: u64,
    #[serde(default)]
    pub protected_vaults: Vec<VaultId>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Principal {
    pub id: String,
    pub issuer: String,
    pub subject: String,
    pub email: String,
    pub vault: VaultId,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Client {
    pub redirect_uris: Vec<String>,
    pub expires: i64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Session {
    pub principal: String,
    pub client: Option<String>,
    pub access: String,
    pub refresh: Option<String>,
    pub access_expires: i64,
    pub expires: i64,
}

pub(super) struct Store {
    vault: Arc<dyn VaultHandle>,
    secret: SecretId,
    pub state: Mutex<State>,
    revision: AtomicU64,
    pub enrolled: tokio::sync::Notify,
}

impl Store {
    pub async fn open(
        vault: Arc<dyn VaultHandle>,
        config: &AuthConfig,
        harness: &dyn ExoHarness,
    ) -> Result<Self> {
        let deployment = format!(
            "{}|{}|{}",
            config.public_url, config.oidc.issuer, config.oidc.client_id
        );
        let provider_store = Self::find(harness).await?;
        let protected = vault.record().id;
        let vault = provider_store
            .as_ref()
            .map(|store| store.vault.clone())
            .unwrap_or(vault);
        let existing = vault
            .list_secrets()
            .await?
            .into_iter()
            .find(|s| s.name == STATE_SECRET);
        let (secret, state) = if let Some(secret) = existing {
            let Some(Secret::Key { value }) = vault.get_secret(&secret.id).await? else {
                anyhow::bail!("invalid server auth state");
            };
            let state: State = serde_json::from_str(&value).context("reading server auth state")?;
            ensure!(
                state.deployment == deployment || state.deployment == "standalone",
                "auth vault belongs to a different server or OIDC client"
            );
            ensure!(
                state
                    .principals
                    .get(&state.owner)
                    .is_none_or(|p| p.email.eq_ignore_ascii_case(&config.owner_email)),
                "owner_email is already bound; use the original owner email"
            );
            (secret.id, state)
        } else {
            let state = State {
                deployment: deployment.clone(),
                owner: Uuid7::now().to_string(),
                ..State::default()
            };
            let secret = vault
                .put_secret(PutSecretRequest {
                    name: STATE_SECRET.into(),
                    policy: None,
                    secret: Secret::Key {
                        value: serde_json::to_string(&state)?,
                    },
                })
                .await?;
            (secret, state)
        };
        let mut store = Self {
            vault,
            secret,
            state: Mutex::new(state),
            revision: AtomicU64::new(0),
            enrolled: tokio::sync::Notify::new(),
        };
        {
            let mut state = store.state.get_mut().clone();
            if state.deployment == "standalone"
                && let Some(owner) = state
                    .principals
                    .values()
                    .find(|p| p.email.eq_ignore_ascii_case(&config.owner_email))
            {
                state.owner = owner.id.clone();
            }
            state.deployment = deployment;
            if !state.protected_vaults.contains(&protected) {
                state.protected_vaults.push(protected);
            }
            store.save(&state).await?;
            *store.state.get_mut() = state;
        }
        store.publish(harness).await?;
        Ok(store)
    }

    pub fn vault_id(&self) -> VaultId {
        self.vault.record().id
    }

    pub async fn find(harness: &dyn ExoHarness) -> Result<Option<Self>> {
        let Some(bytes) = harness.provider_storage()?.get(STORE_LOCATION).await? else {
            return Ok(None);
        };
        let location: StoreLocation = serde_json::from_slice(&bytes)?;
        let vault = exoharness::vault::require_vault(harness, &location.vault).await?;
        let Some(Secret::Key { value }) = vault.get_secret(&location.secret).await? else {
            anyhow::bail!("provider identity store is missing");
        };
        Ok(Some(Self {
            vault,
            secret: location.secret,
            state: Mutex::new(serde_json::from_str(&value)?),
            revision: AtomicU64::new(0),
            enrolled: tokio::sync::Notify::new(),
        }))
    }

    async fn publish(&self, harness: &dyn ExoHarness) -> Result<()> {
        harness
            .provider_storage()?
            .put(
                STORE_LOCATION,
                serde_json::to_vec(&StoreLocation {
                    vault: self.vault_id(),
                    secret: self.secret,
                })?,
                false,
            )
            .await
    }

    pub async fn standalone(harness: &dyn ExoHarness) -> Result<Self> {
        if let Some(store) = Self::find(harness).await? {
            return Ok(store);
        }
        let vault = match harness
            .list_vaults()
            .await?
            .into_iter()
            .find(|v| v.record().name == "exo-server")
        {
            Some(vault) => vault,
            None => harness.create_vault("exo-server").await?,
        };
        if let Some(secret) = vault
            .list_secrets()
            .await?
            .into_iter()
            .find(|s| s.name == STATE_SECRET)
        {
            let Some(Secret::Key { value }) = vault.get_secret(&secret.id).await? else {
                anyhow::bail!("invalid provider identity state");
            };
            let store = Self {
                vault,
                secret: secret.id,
                state: Mutex::new(serde_json::from_str(&value)?),
                revision: AtomicU64::new(0),
                enrolled: tokio::sync::Notify::new(),
            };
            store.publish(harness).await?;
            return Ok(store);
        }
        let state = State {
            deployment: "standalone".into(),
            owner: Uuid7::now().to_string(),
            protected_vaults: vec![vault.record().id],
            ..Default::default()
        };
        let secret = vault
            .put_secret(PutSecretRequest {
                name: STATE_SECRET.into(),
                policy: None,
                secret: Secret::Key {
                    value: serde_json::to_string(&state)?,
                },
            })
            .await?;
        let store = Self {
            vault,
            secret,
            state: Mutex::new(state),
            revision: AtomicU64::new(0),
            enrolled: tokio::sync::Notify::new(),
        };
        store.publish(harness).await?;
        Ok(store)
    }

    pub async fn refresh(&self) -> Result<()> {
        let revision = self
            .vault
            .list_secrets()
            .await?
            .into_iter()
            .find(|secret| secret.id == self.secret)
            .context("provider identity state is missing")?
            .revision;
        if revision <= self.revision.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut state = self.state.lock().await;
        if revision <= self.revision.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(Secret::Key { value }) = self.vault.get_secret(&self.secret).await? else {
            anyhow::bail!("provider identity state is missing");
        };
        *state = serde_json::from_str(&value)?;
        self.revision.store(revision, Ordering::Release);
        Ok(())
    }

    pub async fn save(&self, state: &State) -> Result<()> {
        let metadata = self
            .vault
            .update_secret(
                &self.secret,
                Secret::Key {
                    value: serde_json::to_string(state)?,
                }
                .into(),
            )
            .await?;
        self.revision.store(metadata.revision, Ordering::Release);
        Ok(())
    }

    pub async fn enroll(
        &self,
        mut identity: Identity,
        config: &AuthConfig,
        harness: &dyn ExoHarness,
    ) -> Result<Principal> {
        identity.email = identity.email.trim().to_ascii_lowercase();
        ensure!(
            config.admitted(&identity.email),
            "this identity is not admitted to this server"
        );
        let mut stored = self.state.lock().await;
        if let Some(principal) = stored
            .principals
            .values_mut()
            .find(|p| p.email.eq_ignore_ascii_case(&identity.email))
        {
            if principal.issuer.is_empty() && principal.subject.is_empty() {
                principal.issuer = identity.issuer;
                principal.subject = identity.subject;
                let enrolled = principal.clone();
                self.save(&stored).await?;
                self.enrolled.notify_waiters();
                return Ok(enrolled);
            }
            ensure!(
                principal.issuer == identity.issuer && principal.subject == identity.subject,
                "email is already bound to another identity; contact the server operator"
            );
            return Ok(principal.clone());
        }
        if let Some(principal) = stored
            .principals
            .values()
            .find(|p| p.issuer == identity.issuer && p.subject == identity.subject)
        {
            ensure!(
                config.admitted(&principal.email),
                "identity admission has been removed"
            );
            return Ok(principal.clone());
        }
        let id = if config.owner_email.eq_ignore_ascii_case(&identity.email) {
            stored.owner.clone()
        } else {
            Uuid7::now().to_string()
        };
        let vault = harness.create_vault(&format!("personal-{id}")).await?;
        let principal = Principal {
            id: id.clone(),
            issuer: identity.issuer,
            subject: identity.subject,
            email: identity.email,
            vault: vault.record().id,
        };
        let mut next = stored.clone();
        next.vault_owners.insert(principal.vault, id.clone());
        next.vault_names.insert(principal.vault, "personal".into());
        next.principals.insert(id, principal.clone());
        if let Err(error) = self.save(&next).await {
            harness
                .delete_vault(&vault.record().id)
                .await
                .context("rolling back personal vault after enrollment failed")?;
            return Err(error);
        }
        *stored = next;
        self.enrolled.notify_waiters();
        Ok(principal)
    }
}
