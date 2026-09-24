use super::*;
use crate::types::default_e2b_template;

#[derive(Deserialize)]
struct MigrationBinding {
    record: MigrationBindingRecord,
}

#[derive(Deserialize)]
struct MigrationBindingRecord {
    id: BindingId,
    r#type: BindingType,
    name: String,
    created_at: crate::DateTimeUtc,
    binding: StoredBindingVersion,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredBindingVersion {
    Current(Binding),
    Legacy(LegacyBinding),
}

impl BasicExoHarness {
    pub(super) async fn migrate_legacy_secrets(&self, root: PathBuf) -> Result<()> {
        let _lock = tokio::task::spawn_blocking(move || {
            crate::secrets::lock_secret_file(&root.join("vaults/migration.lock"))
        })
        .await??;
        let storage = &self.inner.storage;
        let marker = "vaults/legacy-migration.json";
        if storage.get_json_if_exists::<bool>(marker).await? == Some(true) {
            return Ok(());
        }
        let keys = storage.list_keys(Path::new("")).await?;
        let mut references = HashMap::new();
        let mut old_secrets = Vec::new();
        for key in &keys {
            let path = Path::new(key);
            if path.extension().is_none_or(|ext| ext != "json")
                || path
                    .parent()
                    .and_then(Path::file_name)
                    .is_none_or(|dir| dir != "secrets")
            {
                continue;
            }
            let scope = path
                .parent()
                .and_then(Path::parent)
                .context("invalid secret path")?;
            let parts: Vec<_> = scope
                .iter()
                .map(|p| p.to_str().context("invalid secret path"))
                .collect::<Result<_>>()?;
            let name = match parts.as_slice() {
                [] => "global".to_owned(),
                ["agents", agent_id] => format!("agent:{agent_id}"),
                ["agents", _, "conversations", thread_id] => format!("thread:{thread_id}"),
                _ => bail!("unrecognized legacy secret scope: {}", scope.display()),
            };
            let vault = match self
                .list_vaults()
                .await?
                .into_iter()
                .find(|v| v.record().name == name)
            {
                Some(vault) => vault,
                None => self.create_vault(&name).await?,
            };
            let record: StoredSecret = storage.get_json(path).await?;
            let reference = SecretReference {
                vault_id: vault.record().id,
                secret_id: record.metadata.id,
            };
            let secret = self.inner.secret_cipher.decrypt_secret(&record.secret)?;
            self.inner
                .vaults
                .import_secret(reference.vault_id, record.metadata, secret)
                .await?;
            references.insert(
                (scope.to_path_buf(), reference.secret_id),
                reference.clone(),
            );
            if !parts.is_empty() {
                let record_path = scope.join("record.json");
                if parts.len() == 2 {
                    let mut record: AgentRecord = storage.get_json(&record_path).await?;
                    if !record.vaults.contains(&reference.vault_id) {
                        record.vaults.push(reference.vault_id);
                        storage.put_json(&record_path, &record).await?;
                    }
                } else {
                    let mut record: ConversationRecord = storage.get_json(&record_path).await?;
                    if !record.vaults.contains(&reference.vault_id) {
                        record.vaults.push(reference.vault_id);
                        storage.put_json(&record_path, &record).await?;
                    }
                }
            }
            old_secrets.push(path);
        }
        for key in &keys {
            let path = Path::new(key);
            if path.extension().is_none_or(|ext| ext != "json")
                || path
                    .parent()
                    .and_then(Path::file_name)
                    .is_none_or(|dir| dir != "bindings")
            {
                continue;
            }
            let record = storage.get_json::<MigrationBinding>(path).await?.record;
            let binding = match record.binding {
                StoredBindingVersion::Current(_binding) => continue,
                StoredBindingVersion::Legacy(binding) => binding,
            };
            let scope = path
                .parent()
                .and_then(Path::parent)
                .context("invalid binding path")?;
            let binding = binding.migrate(|id| {
                let mut scopes = vec![scope.to_path_buf()];
                if scope.components().count() == 4 {
                    scopes.push(scope.ancestors().nth(2).unwrap().to_path_buf());
                }
                scopes.push(PathBuf::new());
                scopes
                    .into_iter()
                    .find_map(|scope| references.get(&(scope, id)).cloned())
                    .with_context(|| {
                        format!(
                            "legacy binding {} references missing secret {id}",
                            record.id
                        )
                    })
            })?;
            storage
                .put_json(
                    path,
                    &StoredBinding {
                        record: BindingRecord {
                            id: record.id,
                            r#type: record.r#type,
                            name: record.name,
                            created_at: record.created_at,
                            binding,
                        },
                    },
                )
                .await?;
        }
        for path in old_secrets {
            storage.delete_key_if_exists(path).await?;
        }
        storage.put_json(marker, &true).await?;
        Ok(())
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum LegacyBinding {
    Env {
        name: String,
        env_var: String,
        secret_id: SecretId,
    },
    Mcp {
        name: String,
        server_url: String,
        secret_id: Option<SecretId>,
    },
    Llm {
        name: String,
        model: String,
        base_url: Option<String>,
        secret_id: Option<SecretId>,
    },
    Sandbox {
        name: String,
        config: LegacySandboxProviderConfig,
    },
}
#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
#[serde(tag = "provider", rename_all = "lowercase", deny_unknown_fields)]
enum LegacySandboxProviderConfig {
    Daytona {
        api_key_secret_id: SecretId,
        #[serde(default)]
        region: Option<String>,
        #[serde(default)]
        organization_id: Option<String>,
        #[serde(default)]
        api_url: Option<String>,
        #[serde(default = "crate::sandbox_provider::default_daytona_image")]
        default_image: String,
    },
    Vercel {
        api_token_secret_id: SecretId,
        team_id: String,
        project_id: String,
        #[serde(default)]
        api_url: Option<String>,
        #[serde(default = "crate::sandbox_provider::default_vercel_image")]
        default_image: String,
    },
    E2b {
        api_key_secret_id: SecretId,
        #[serde(default)]
        api_url: Option<String>,
        #[serde(default = "default_e2b_template")]
        default_image: String,
    },
    Sprites {
        token_secret_id: SecretId,
        #[serde(default)]
        api_url: Option<String>,
        #[serde(default)]
        url_auth: Option<String>,
        #[serde(default)]
        organization: Option<String>,
        #[serde(default)]
        labels: Vec<String>,
    },
}

impl LegacyBinding {
    fn migrate(self, resolve: impl Fn(SecretId) -> Result<SecretReference>) -> Result<Binding> {
        Ok(match self {
            Self::Env {
                name,
                env_var,
                secret_id,
            } => Binding::Env {
                name,
                env_var,
                secret: resolve(secret_id)?,
            },
            Self::Mcp {
                name,
                server_url,
                secret_id,
            } => Binding::Mcp {
                name,
                server_url,
                secret: secret_id.map(&resolve).transpose()?,
            },
            Self::Llm {
                name,
                model,
                base_url,
                secret_id,
            } => Binding::Llm {
                name,
                model,
                base_url,
                secret: secret_id.map(&resolve).transpose()?,
            },
            Self::Sandbox { name, config } => Binding::Sandbox {
                name,
                config: config.migrate(resolve)?,
            },
        })
    }
}

impl LegacySandboxProviderConfig {
    fn migrate(
        self,
        resolve: impl Fn(SecretId) -> Result<SecretReference>,
    ) -> Result<SandboxProviderConfig> {
        Ok(match self {
            Self::Daytona {
                api_key_secret_id,
                region,
                organization_id,
                api_url,
                default_image,
            } => SandboxProviderConfig::Daytona {
                api_key_secret: resolve(api_key_secret_id)?,
                region,
                organization_id,
                api_url,
                default_image,
            },
            Self::Vercel {
                api_token_secret_id,
                team_id,
                project_id,
                api_url,
                default_image,
            } => SandboxProviderConfig::Vercel {
                api_token_secret: resolve(api_token_secret_id)?,
                team_id,
                project_id,
                api_url,
                default_image,
            },
            Self::E2b {
                api_key_secret_id,
                api_url,
                default_image,
            } => SandboxProviderConfig::E2b {
                api_key_secret: resolve(api_key_secret_id)?,
                api_url,
                default_image,
            },
            Self::Sprites {
                token_secret_id,
                api_url,
                url_auth,
                organization,
                labels,
            } => SandboxProviderConfig::Sprites {
                token_secret: resolve(token_secret_id)?,
                api_url,
                url_auth,
                organization,
                labels,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vault_migration_keeps_scope_ids_and_bindings_across_retry() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let config = crate::test_support::local_test_config(temp.path());
        let storage = BasicObjectStore::local_filesystem(temp.path()).await?;
        let cipher = build_secret_cipher(
            config.secret_backend.clone(),
            temp.path().to_string_lossy().into_owned(),
        )?;
        let agent_id = Uuid7::now();
        let thread_id = Uuid7::now();
        let agent_path = format!("agents/{agent_id}");
        let thread_path = format!("{agent_path}/conversations/{thread_id}");
        storage
            .put_json(
                format!("{agent_path}/record.json"),
                &AgentRecord {
                    id: agent_id,
                    name: "agent".into(),
                    slug: "agent".into(),
                    vaults: vec![],
                },
            )
            .await?;
        storage
            .put_json(
                format!("{thread_path}/record.json"),
                &ConversationRecord {
                    id: thread_id,
                    name: "thread".into(),
                    slug: "thread".into(),
                    latest_event_id: None,
                    vaults: vec![],
                },
            )
            .await?;
        #[derive(Serialize)]
        struct LegacyRecord {
            record: LegacyFields,
        }
        #[derive(Serialize)]
        struct LegacyFields {
            id: BindingId,
            r#type: BindingType,
            name: String,
            created_at: crate::DateTimeUtc,
            binding: LegacyBinding,
        }
        let mut fixtures = Vec::new();
        for scope in ["", agent_path.as_str(), thread_path.as_str()] {
            let secret_id = Uuid7::now();
            let binding_id = Uuid7::now();
            let key = Secret::Key {
                value: format!("key-{secret_id}"),
            };
            let secret_path = Path::new(scope).join(format!("secrets/{secret_id}.json"));
            storage
                .put_json(
                    &secret_path,
                    &StoredSecret {
                        metadata: SecretMetadata {
                            id: secret_id,
                            name: "provider".into(),
                            r#type: crate::SecretType::Key,
                            created_at: Utc::now(),
                            target: None,
                            revision: 1,
                        },
                        secret: cipher.encrypt_secret(&key)?,
                    },
                )
                .await?;
            let binding_path = Path::new(scope).join(format!("bindings/{binding_id}.json"));
            storage
                .put_json(
                    &binding_path,
                    &LegacyRecord {
                        record: LegacyFields {
                            id: binding_id,
                            r#type: BindingType::Llm,
                            name: "model".into(),
                            created_at: Utc::now(),
                            binding: LegacyBinding::Llm {
                                name: "model".into(),
                                model: "gpt-5.6-sol".into(),
                                base_url: None,
                                secret_id: Some(secret_id),
                            },
                        },
                    },
                )
                .await?;
            fixtures.push((secret_path, binding_path, secret_id, key));
        }
        let invalid = Path::new("bindings/invalid.json");
        storage
            .put_json(
                invalid,
                &LegacyRecord {
                    record: LegacyFields {
                        id: Uuid7::now(),
                        r#type: BindingType::Env,
                        name: "invalid".into(),
                        created_at: Utc::now(),
                        binding: LegacyBinding::Env {
                            name: "invalid".into(),
                            env_var: "TOKEN".into(),
                            secret_id: Uuid7::now(),
                        },
                    },
                },
            )
            .await?;
        assert!(BasicExoHarness::new(config.clone()).await.is_err());
        for (path, _, _, _) in &fixtures {
            assert!(temp.path().join(path).exists());
        }
        storage.delete_key_if_exists(invalid).await?;
        let harness = BasicExoHarness::new(config.clone()).await?;
        let global = global_vault(&harness).await?;
        let agent = harness.get_agent(&agent_id).await?.unwrap();
        let thread = agent.get_thread(&thread_id).await?.unwrap();
        let ids = [
            global.record().id,
            agent.record().vaults[0],
            thread.record().vaults[0],
        ];
        for ((old_path, binding_path, secret_id, key), vault_id) in fixtures.iter().zip(ids) {
            assert!(!temp.path().join(old_path).exists());
            let record: StoredBinding = storage.get_json(binding_path).await?;
            let Binding::Llm {
                secret: Some(reference),
                ..
            } = record.record.binding
            else {
                panic!("missing migrated model binding")
            };
            assert_eq!(
                reference,
                SecretReference {
                    vault_id,
                    secret_id: *secret_id
                }
            );
            assert_eq!(
                thread
                    .get_vault(&vault_id)
                    .await?
                    .unwrap()
                    .get_secret(secret_id)
                    .await?,
                Some(key.clone())
            );
        }
        assert!(global.get_secret(&fixtures[1].2).await?.is_none());
        assert!(agent.get_vault(&ids[2]).await?.is_none());
        let reopened = BasicExoHarness::new(config).await?;
        assert_eq!(reopened.list_vaults().await?.len(), 3);
        Ok(())
    }
}
