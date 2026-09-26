use super::oidc::{AuthConfig, Identity};
use anyhow::{Context, Result, ensure};
use exoharness::{
    ExoHarness, PutSecretRequest, Secret, SecretId, Uuid7,
    vault::{VaultHandle, VaultId},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;

pub(super) const STATE_SECRET: &str = "exo-server-state";

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
    pub enrolled: tokio::sync::Notify,
}

impl Store {
    pub async fn open(vault: Arc<dyn VaultHandle>, config: &AuthConfig) -> Result<Self> {
        let deployment = format!(
            "{}|{}|{}",
            config.public_url, config.oidc.issuer, config.oidc.client_id
        );
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
                state.deployment == deployment,
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
                deployment,
                owner: Uuid7::now().to_string(),
                ..State::default()
            };
            let secret = vault
                .put_secret(PutSecretRequest {
                    name: STATE_SECRET.into(),
                    target: None,
                    secret: Secret::Key {
                        value: serde_json::to_string(&state)?,
                    },
                })
                .await?;
            (secret, state)
        };
        Ok(Self {
            vault,
            secret,
            state: Mutex::new(state),
            enrolled: tokio::sync::Notify::new(),
        })
    }

    pub fn vault_id(&self) -> VaultId {
        self.vault.record().id
    }

    pub async fn save(&self, state: &State) -> Result<()> {
        self.vault
            .update_secret(
                &self.secret,
                Secret::Key {
                    value: serde_json::to_string(state)?,
                }
                .into(),
            )
            .await?;
        Ok(())
    }

    pub async fn enroll(
        &self,
        identity: Identity,
        config: &AuthConfig,
        harness: &dyn ExoHarness,
    ) -> Result<Principal> {
        ensure!(
            config.admitted(&identity.email),
            "this identity is not admitted to this server"
        );
        let mut stored = self.state.lock().await;
        if let Some(principal) = stored
            .principals
            .values()
            .find(|p| p.email == identity.email)
        {
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
