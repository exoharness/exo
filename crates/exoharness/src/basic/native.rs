use super::*;
use crate::SandboxProviderConfig;
#[cfg(test)]
use crate::SnapshotFormat;
use crate::sandbox::{LocalProcessSandboxBackend, ManagedSandboxBackend};
#[cfg(feature = "apple-keychain")]
use crate::secrets::AppleKeychainSecretKeyProvider;
use crate::secrets::{
    EncryptedSecret, FileBackedSecretKeyProvider, SecretCipher, SecretKeyProvider,
    StaticSecretKeyProvider,
};
use crate::vault::SecretReference;

pub(super) struct NativeState {
    pub(super) cache_root: PathBuf,
    pub(super) durable_file_system_root: PathBuf,
    pub(super) resources: crate::resources::ResourceStore,
    pub(super) secret_cipher: SecretCipher,
}

#[derive(Debug, Clone)]
pub enum SecretBackendChoice {
    #[cfg(feature = "apple-keychain")]
    AppleKeychain,
    File {
        path: Option<PathBuf>,
    },
    Static([u8; 32]),
}

impl SecretBackendChoice {
    fn master_key_path(&self, root: &Path) -> Option<PathBuf> {
        match self {
            Self::File { path } => Some(path.clone().unwrap_or_else(|| root.join("master.key"))),
            _ => None,
        }
    }
}

impl SandboxBackendRegistration {
    pub fn from_builtin_provider(provider: SandboxProvider) -> Result<Self> {
        match provider.as_str() {
            "apple_container" => Ok(Self::apple_container()),
            "aws_agentcore" => Ok(Self::aws_agentcore()),
            "daytona" => Ok(Self::daytona(
                DaytonaBackendSpec::with_conventional_secrets(),
            )),
            "docker" => Ok(Self::docker()),
            "e2b" => Ok(Self::e2b(E2bBackendSpec::default())),
            "firecracker" => Ok(Self::firecracker(FirecrackerBackendSpec::default())),
            "local_process" => Ok(Self::local_process()),
            "smolvm" => Ok(Self::smolvm()),
            "sprites" => Ok(Self::sprites(SpritesBackendSpec::default())),
            "vercel" => Ok(Self::vercel(VercelBackendSpec::with_conventional_secrets())),
            _ => bail!("sandbox provider {provider} is not built into exoharness"),
        }
    }

    pub fn apple_container() -> Self {
        Self::from_factory(SandboxProvider::AppleContainer, true, false, |inner| {
            Box::pin(async move {
                Ok(Arc::new(
                    crate::CliContainerSandboxBackend::apple_container()
                        .with_durable_file_system_root(
                            inner.native.durable_file_system_root.clone(),
                        ),
                ) as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn docker() -> Self {
        Self::from_factory(SandboxProvider::Docker, true, false, |inner| {
            Box::pin(async move {
                Ok(Arc::new(
                    crate::CliContainerSandboxBackend::docker().with_durable_file_system_root(
                        inner.native.durable_file_system_root.clone(),
                    ),
                ) as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    #[cfg(feature = "firecracker")]
    pub fn firecracker(spec: FirecrackerBackendSpec) -> Self {
        Self::from_factory(
            SandboxProvider::Firecracker,
            cfg!(any(target_os = "linux", target_os = "macos")),
            true,
            move |inner| {
                let spec = spec.clone();
                let resolver = Arc::new(LocalEgressResolver {
                    harness: Arc::downgrade(inner),
                });
                Box::pin(crate::firecracker_backend_with_credentials(
                    spec.config,
                    spec.lima,
                    resolver,
                ))
            },
        )
    }

    #[cfg(not(feature = "firecracker"))]
    pub fn firecracker(_spec: FirecrackerBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Firecracker, false, true, |_| {
            Box::pin(async move {
                bail!("Firecracker support requires building Exo with --features firecracker")
            })
        })
    }

    pub fn local_process() -> Self {
        Self::from_backend(
            SandboxProvider::LocalProcess,
            Arc::new(LocalProcessSandboxBackend::new()),
        )
    }

    /// Local microVM sandboxes via the `smolvm` CLI. Unlike `docker` this puts a
    /// hypervisor boundary around the workload, and unlike `apple_container` it
    /// is not macOS-only.
    pub fn smolvm() -> Self {
        // A factory, not a fixed backend: the binary paths are configured per
        // binding (`exo environment provider create --backend smolvm --smolvm-binary`),
        // so they have to be read when a request arrives rather than at startup —
        // the same shape daytona/e2b use for their credentials. The result is
        // cached per provider by `sandbox_backend_for_provider`, so this runs
        // once per harness and not once per sandbox.
        Self::from_factory(SandboxProvider::Smolvm, true, true, |inner| {
            Box::pin(async move {
                let config = inner.smolvm_config_from_binding().await?;
                let resolver = Arc::new(LocalEgressResolver {
                    harness: Arc::downgrade(inner),
                });
                Ok(Arc::new(
                    crate::SmolvmSandboxBackend::from_config(config).with_credentials(resolver),
                ) as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn daytona(spec: DaytonaBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Daytona, false, false, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.daytona_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.daytona_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::DaytonaSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn e2b(spec: E2bBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::E2b, false, false, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.e2b_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.e2b_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::E2bSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn sprites(spec: SpritesBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Sprites, false, false, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.sprites_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.sprites_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::SpritesSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn vercel(spec: VercelBackendSpec) -> Self {
        Self::from_factory(SandboxProvider::Vercel, false, false, move |inner| {
            let spec = spec.clone();
            Box::pin(async move {
                let config = match inner.vercel_config_from_binding().await? {
                    Some(config) => config,
                    None => inner.vercel_config_from_spec(&spec).await?,
                };
                Ok(Arc::new(crate::VercelSandboxBackend::new(config)?)
                    as Arc<dyn ManagedSandboxBackend>)
            })
        })
    }

    pub fn aws_agentcore() -> Self {
        Self::from_factory(SandboxProvider::AwsAgentCore, false, false, |_inner| {
            Box::pin(async move {
                #[cfg(feature = "aws-agentcore")]
                {
                    let config = _inner.aws_agentcore_config_from_binding().await?.ok_or_else(|| {
                        anyhow!(
                            "aws-agentcore sandbox requested but no sandbox provider binding is configured; run `exo environment provider create --backend aws-agentcore --runtime-arn <arn>`"
                        )
                    })?;
                    Ok(
                        Arc::new(crate::AwsAgentCoreSandboxBackend::new(config).await?)
                            as Arc<dyn ManagedSandboxBackend>,
                    )
                }
                #[cfg(not(feature = "aws-agentcore"))]
                {
                    bail!(
                        "aws-agentcore sandbox backend requires building exoharness with the aws-agentcore feature"
                    );
                }
            })
        })
    }
}

/// Complete Firecracker configuration supplied by the frontend.
#[derive(Debug, Clone, Default)]
pub struct FirecrackerBackendSpec {
    #[cfg(feature = "firecracker")]
    pub config: crate::FirecrackerConfig,
    #[cfg(feature = "firecracker")]
    pub lima: crate::FirecrackerLimaConfig,
}

/// Daytona connection config plus the secret-store names for its credentials,
/// resolved lazily so the harness can advertise Daytona before any are set.
#[derive(Debug, Clone)]
pub struct DaytonaBackendSpec {
    pub api_url: String,
    pub toolbox_url: String,
    /// Secret holding the API key (required at first use).
    pub api_key_secret: String,
    pub organization_id_secret: Option<String>,
    pub target_secret: Option<String>,
}

impl Default for DaytonaBackendSpec {
    /// Official endpoints; credentials read from the conventional `DAYTONA_*`
    /// secret names.
    fn default() -> Self {
        Self {
            api_url: crate::DEFAULT_DAYTONA_API_URL.to_string(),
            toolbox_url: crate::DEFAULT_DAYTONA_TOOLBOX_URL.to_string(),
            api_key_secret: "DAYTONA_API_KEY".to_string(),
            organization_id_secret: Some("DAYTONA_ORGANIZATION_ID".to_string()),
            target_secret: Some("DAYTONA_TARGET".to_string()),
        }
    }
}

impl DaytonaBackendSpec {
    /// Official endpoints; credentials read from the conventional `DAYTONA_*`
    /// secret names.
    pub fn with_conventional_secrets() -> Self {
        Self::default()
    }
}

/// E2B connection config plus secret-store names for credentials, resolved
/// lazily on first use.
#[derive(Debug, Clone)]
pub struct E2bBackendSpec {
    pub api_url: String,
    pub api_key_secret: String,
    pub template_id: String,
}

impl Default for E2bBackendSpec {
    fn default() -> Self {
        Self {
            api_url: crate::DEFAULT_E2B_API_URL.to_string(),
            api_key_secret: "E2B_API_KEY".to_string(),
            template_id: crate::default_e2b_template(),
        }
    }
}

/// Sprites connection config plus secret-store name for the bearer token,
/// resolved lazily on first use.
#[derive(Debug, Clone)]
pub struct SpritesBackendSpec {
    pub api_url: String,
    pub token_secret: String,
    pub url_auth: Option<String>,
    pub organization: Option<String>,
    pub labels: Vec<String>,
}

impl Default for SpritesBackendSpec {
    fn default() -> Self {
        Self {
            api_url: crate::DEFAULT_SPRITES_API_URL.to_string(),
            token_secret: "SPRITES_TOKEN".to_string(),
            url_auth: None,
            organization: None,
            labels: Vec::new(),
        }
    }
}

/// Vercel connection config plus the secret-store names for its credentials,
/// resolved lazily so the harness can advertise Vercel before any are set.
#[derive(Debug, Clone)]
pub struct VercelBackendSpec {
    pub api_url: String,
    pub api_token_secret: String,
    pub team_id_secret: String,
    pub project_id_secret: String,
}

impl VercelBackendSpec {
    /// Official endpoint; credentials read from conventional `VERCEL_*` secret
    /// names.
    pub fn with_conventional_secrets() -> Self {
        Self {
            api_url: crate::DEFAULT_VERCEL_API_URL.to_string(),
            api_token_secret: "VERCEL_TOKEN".to_string(),
            team_id_secret: "VERCEL_TEAM_ID".to_string(),
            project_id_secret: "VERCEL_PROJECT_ID".to_string(),
        }
    }
}

// TODO: as more knobs land here, swap to a builder pattern.
#[derive(Clone)]
pub struct BasicExoHarnessConfig {
    pub root: PathBuf,
    pub secret_backend: SecretBackendChoice,
    /// Default when a caller doesn't request a provider. Must be in `sandbox_backends`.
    pub sandbox_default: SandboxProvider,
    pub sandbox_policy: Option<crate::EgressPolicy>,
    /// Supported providers; anything not listed is rejected.
    pub sandbox_backends: Vec<SandboxBackendRegistration>,
}

impl BasicExoHarnessInner {
    /// `DaytonaConfig` from the conventional `DAYTONA_*` secret-name spec.
    pub(super) async fn daytona_config_from_spec(
        &self,
        spec: &DaytonaBackendSpec,
    ) -> Result<crate::DaytonaConfig> {
        let api_key = self
            .secret_key(&spec.api_key_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "daytona sandbox requested but secret {:?} is not set",
                    spec.api_key_secret
                )
            })?;
        let organization_id = match &spec.organization_id_secret {
            Some(name) => self.secret_key(name).await?,
            None => None,
        };
        let target = match &spec.target_secret {
            Some(name) => self.secret_key(name).await?,
            None => None,
        };
        Ok(crate::DaytonaConfig {
            api_key,
            api_url: spec.api_url.clone(),
            toolbox_url: spec.toolbox_url.clone(),
            target,
            organization_id,
        })
    }

    pub(super) async fn e2b_config_from_spec(
        &self,
        spec: &E2bBackendSpec,
    ) -> Result<crate::E2bConfig> {
        let api_key = self
            .secret_key(&spec.api_key_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "e2b sandbox requested but secret {:?} is not set",
                    spec.api_key_secret
                )
            })?;
        Ok(crate::E2bConfig {
            api_key,
            api_url: spec.api_url.clone(),
            template_id: spec.template_id.clone(),
            envd_port: crate::DEFAULT_E2B_ENVD_PORT,
            envd_base_url: None,
            secure: false,
        })
    }

    pub(super) async fn e2b_config_from_binding(&self) -> Result<Option<crate::E2bConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((api_key_secret, api_url, default_image)) =
            bindings
                .into_iter()
                .rev()
                .find_map(|record| match record.binding {
                    Binding::Sandbox {
                        config:
                            SandboxProviderConfig::E2b {
                                api_key_secret,
                                api_url,
                                default_image,
                            },
                        ..
                    } => Some((api_key_secret, api_url, default_image)),
                    _ => None,
                })
        else {
            return Ok(None);
        };
        let api_key = self
            .secret_key_by_id(&api_key_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "e2b sandbox binding references secret id {api_key_secret:?}, which is not set"
                )
            })?;
        Ok(Some(crate::E2bConfig {
            api_key,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_E2B_API_URL.to_string()),
            template_id: default_image,
            envd_port: crate::DEFAULT_E2B_ENVD_PORT,
            envd_base_url: None,
            secure: false,
        }))
    }

    pub(super) async fn sprites_config_from_spec(
        &self,
        spec: &SpritesBackendSpec,
    ) -> Result<crate::SpritesConfig> {
        let token = self.secret_key(&spec.token_secret).await?.ok_or_else(|| {
            anyhow!(
                "sprites sandbox requested but secret {:?} is not set",
                spec.token_secret
            )
        })?;
        Ok(crate::SpritesConfig {
            token,
            api_url: spec.api_url.clone(),
            url_auth: spec.url_auth.clone(),
            organization: spec.organization.clone(),
            extra_labels: spec.labels.clone(),
        })
    }

    pub(super) async fn sprites_config_from_binding(&self) -> Result<Option<crate::SpritesConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((token_secret, api_url, url_auth, organization, labels)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Sprites {
                            token_secret,
                            api_url,
                            url_auth,
                            organization,
                            labels,
                        },
                    ..
                } => Some((token_secret, api_url, url_auth, organization, labels)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let token = self.secret_key_by_id(&token_secret).await?.ok_or_else(|| {
            anyhow!(
                "sprites sandbox binding references secret id {token_secret:?}, which is not set"
            )
        })?;
        Ok(Some(crate::SpritesConfig {
            token,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_SPRITES_API_URL.to_string()),
            url_auth,
            organization,
            extra_labels: labels,
        }))
    }

    /// `VercelConfig` from the conventional `VERCEL_*` secret-name spec.
    pub(super) async fn vercel_config_from_spec(
        &self,
        spec: &VercelBackendSpec,
    ) -> Result<crate::VercelConfig> {
        let api_token = self
            .secret_key(&spec.api_token_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox requested but secret {:?} is not set",
                    spec.api_token_secret
                )
            })?;
        let team_id = self
            .secret_key(&spec.team_id_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox requested but secret {:?} is not set",
                    spec.team_id_secret
                )
            })?;
        let project_id = self
            .secret_key(&spec.project_id_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox requested but secret {:?} is not set",
                    spec.project_id_secret
                )
            })?;
        Ok(crate::VercelConfig {
            api_token,
            api_url: spec.api_url.clone(),
            team_id,
            project_id,
        })
    }

    /// Decrypt the first stored secret matching `predicate`, if any.
    pub(super) async fn find_secret_by(
        &self,
        predicate: impl Fn(&SecretMetadata) -> bool,
    ) -> Result<Option<Secret>> {
        let vault = self.vaults.global_vault().await?;
        let metadata = vault.list_secrets().await?.into_iter().find(predicate);
        match metadata {
            Some(metadata) => vault.get_secret(&metadata.id).await,
            None => Ok(None),
        }
    }

    /// Decrypt the `Key`-typed secret stored under `name`, if it exists.
    pub(super) async fn secret_key(&self, name: &str) -> Result<Option<String>> {
        match self.find_secret_by(|s| s.name == name).await? {
            Some(Secret::Key { value }) => Ok(Some(value)),
            Some(Secret::Oauth { .. } | Secret::GithubCli { .. }) => {
                bail!("secret {name:?} is not a static API key")
            }
            None => Ok(None),
        }
    }

    /// Decrypt the `Key`-typed secret with the given id, if it exists.
    pub(super) async fn secret_key_by_id(
        &self,
        reference: &SecretReference,
    ) -> Result<Option<String>> {
        match self
            .vaults
            .get_vault(&reference.vault_id)
            .await?
            .context("sandbox credential vault is unavailable")?
            .get_secret(&reference.secret_id)
            .await?
        {
            Some(Secret::Key { value }) => Ok(Some(value)),
            Some(Secret::Oauth { .. } | Secret::GithubCli { .. }) => {
                bail!("sandbox credential must be an API key")
            }
            None => Ok(None),
        }
    }

    /// SmolVM settings from the newest root-scoped sandbox binding.
    pub(super) async fn smolvm_config_from_binding(&self) -> Result<crate::SmolvmBackendConfig> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let mut config = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Smolvm {
                            binary,
                            boot_binary,
                            storage_gib,
                            overlay_gib,
                            ..
                        },
                    ..
                } => Some(crate::SmolvmBackendConfig {
                    binary,
                    boot_binary,
                    storage_gib,
                    overlay_gib,
                    ..Default::default()
                }),
                _ => None,
            })
            .unwrap_or_default();
        config.image_cache = Some(self.native.cache_root.join("smolvm/images"));
        Ok(config)
    }

    pub(super) async fn daytona_config_from_binding(&self) -> Result<Option<crate::DaytonaConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((api_key_secret, region, organization_id, api_url)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Daytona {
                            api_key_secret,
                            region,
                            organization_id,
                            api_url,
                            ..
                        },
                    ..
                } => Some((api_key_secret, region, organization_id, api_url)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let api_key = self
            .secret_key_by_id(&api_key_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "daytona sandbox binding references secret id {api_key_secret:?}, \
                 which is not set"
                )
            })?;
        Ok(Some(crate::DaytonaConfig {
            api_key,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_DAYTONA_API_URL.to_string()),
            toolbox_url: crate::DEFAULT_DAYTONA_TOOLBOX_URL.to_string(),
            target: region,
            organization_id,
        }))
    }

    pub(super) async fn vercel_config_from_binding(&self) -> Result<Option<crate::VercelConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((api_token_secret, team_id, project_id, api_url)) = bindings
            .into_iter()
            .rev()
            .find_map(|record| match record.binding {
                Binding::Sandbox {
                    config:
                        SandboxProviderConfig::Vercel {
                            api_token_secret,
                            team_id,
                            project_id,
                            api_url,
                            ..
                        },
                    ..
                } => Some((api_token_secret, team_id, project_id, api_url)),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let api_token = self
            .secret_key_by_id(&api_token_secret)
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "vercel sandbox binding references secret id {api_token_secret:?}, \
                 which is not set"
                )
            })?;
        Ok(Some(crate::VercelConfig {
            api_token,
            api_url: api_url.unwrap_or_else(|| crate::DEFAULT_VERCEL_API_URL.to_string()),
            team_id,
            project_id,
        }))
    }

    #[cfg(feature = "aws-agentcore")]
    pub(super) async fn aws_agentcore_config_from_binding(
        &self,
    ) -> Result<Option<crate::AwsAgentCoreConfig>> {
        let bindings = list_binding_records(&self.storage, Path::new("bindings")).await?;
        let Some((runtime_arn, region, qualifier, endpoint_url, session_storage_mount_path)) =
            bindings
                .into_iter()
                .rev()
                .find_map(|record| match record.binding {
                    Binding::Sandbox {
                        config:
                            SandboxProviderConfig::AwsAgentCore {
                                runtime_arn,
                                region,
                                qualifier,
                                endpoint_url,
                                session_storage_mount_path,
                                ..
                            },
                        ..
                    } => Some((
                        runtime_arn,
                        region,
                        qualifier,
                        endpoint_url,
                        session_storage_mount_path,
                    )),
                    _ => None,
                })
        else {
            return Ok(None);
        };
        let session_storage_mount_path = session_storage_mount_path
            .or_else(|| nonempty_env("AWS_AGENTCORE_SESSION_STORAGE_MOUNT_PATH"))
            .or_else(|| nonempty_env("AGENTCORE_SESSION_STORAGE_MOUNT_PATH"));
        Ok(Some(crate::AwsAgentCoreConfig {
            runtime_arn,
            region,
            qualifier,
            endpoint_url,
            credentials: aws_agentcore_credentials_from_env(),
            session_storage_mount_path,
        }))
    }
}

#[cfg(feature = "aws-agentcore")]
pub(super) fn aws_agentcore_credentials_from_env() -> Option<crate::AwsAgentCoreCredentials> {
    let access_key_id = std::env::var("AWS_AGENTCORE_ACCESS_KEY_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let secret_access_key = std::env::var("AWS_AGENTCORE_SECRET_ACCESS_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())?;
    let session_token = std::env::var("AWS_AGENTCORE_SESSION_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty());

    Some(crate::AwsAgentCoreCredentials {
        access_key_id,
        secret_access_key,
        session_token,
    })
}

#[cfg(feature = "aws-agentcore")]
pub(super) fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

impl BasicExoHarness {
    /// In-memory state uses a fresh encryption key, ignoring `config.secret_backend`.
    pub async fn in_memory(config: BasicExoHarnessConfig) -> Result<Self> {
        Self::new_with_storage(config, None, BasicObjectStore::in_memory(), true).await
    }

    pub async fn new(config: BasicExoHarnessConfig) -> Result<Self> {
        Self::new_with_backend(config, None).await
    }

    #[cfg(test)]
    pub(crate) async fn new_with_sandbox_backend(
        config: BasicExoHarnessConfig,
        sandbox_backend: Arc<dyn ManagedSandboxBackend>,
    ) -> Result<Self> {
        Self::new_with_backend(config, Some(sandbox_backend)).await
    }

    #[cfg(test)]
    pub(crate) async fn daytona_config_from_binding_for_test(
        &self,
    ) -> Result<Option<crate::DaytonaConfig>> {
        self.inner.daytona_config_from_binding().await
    }

    /// `seed` pre-populates the cache for the default provider, letting tests
    /// inject a mock backend.
    pub(super) async fn new_with_backend(
        config: BasicExoHarnessConfig,
        seed: Option<Arc<dyn ManagedSandboxBackend>>,
    ) -> Result<Self> {
        let root = config.root.clone();
        let storage = BasicObjectStore::local_filesystem(&config.root).await?;
        let harness = Self::new_with_storage(config, seed, storage, false).await?;
        harness.migrate_legacy_secrets(root).await?;
        Ok(harness)
    }

    pub(super) async fn new_with_storage(
        config: BasicExoHarnessConfig,
        seed: Option<Arc<dyn ManagedSandboxBackend>>,
        storage: BasicObjectStore,
        in_memory: bool,
    ) -> Result<Self> {
        let BasicExoHarnessConfig {
            root,
            secret_backend,
            sandbox_default,
            sandbox_backends,
            sandbox_policy,
        } = config;

        let mut registry = HashMap::new();
        for choice in sandbox_backends {
            let provider = choice.provider();
            if registry.insert(provider.clone(), choice).is_some() {
                bail!("duplicate sandbox provider {provider:?} in sandbox_backends");
            }
        }
        if !registry.contains_key(&sandbox_default) {
            bail!("default sandbox provider {sandbox_default:?} is not in the supported set");
        }

        let mut cache = HashMap::new();
        if let Some(backend) = seed {
            cache.insert(sandbox_default.clone(), backend);
        }

        let resource_master_key = secret_backend.master_key_path(&root);
        let secret_backend = if in_memory {
            SecretBackendChoice::Static(crate::secrets::random_master_key())
        } else {
            secret_backend
        };
        let secret_cipher = build_secret_cipher(secret_backend, &root);
        let vaults = BasicVaultStore::new(
            (!in_memory).then(|| root.join("vaults")),
            secret_cipher.clone(),
        )?;
        Ok(Self {
            sessions: None,
            caller: None,
            inner: Arc::new(BasicExoHarnessInner {
                turn_queue_locks: Arc::default(),
                access_policy: std::sync::OnceLock::new(),
                vaults,
                storage,
                write_lock: AsyncMutex::new(()),
                resource_locks: Mutex::new(HashMap::new()),
                subscribers: Mutex::new(HashMap::new()),
                sandbox_policy,
                sandbox_registry: registry,
                sandbox_backends: AsyncMutex::new(cache),
                running_sandboxes: AsyncMutex::new(HashMap::new()),
                running_processes: AsyncMutex::new(HashMap::new()),
                host: Arc::new(TokioRuntimeHost),
                native: NativeState {
                    secret_cipher,
                    cache_root: root.join("cache"),
                    durable_file_system_root: root.join("durable-filesystems"),
                    resources: crate::resources::ResourceStore::new(&root)?
                        .excluding_master_key(resource_master_key)?,
                },
            }),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct StoredSecret {
    pub(super) metadata: SecretMetadata,
    pub(super) secret: EncryptedSecret,
}

pub(crate) fn build_secret_cipher(choice: SecretBackendChoice, root: &Path) -> SecretCipher {
    let master_key_path = choice.master_key_path(root);
    let provider: Arc<dyn SecretKeyProvider> = match choice {
        #[cfg(feature = "apple-keychain")]
        SecretBackendChoice::AppleKeychain => Arc::new(AppleKeychainSecretKeyProvider::new(
            root.to_string_lossy().into_owned(),
            root.join("master.keychain.lock"),
        )),
        SecretBackendChoice::File { .. } => Arc::new(FileBackedSecretKeyProvider::new(
            master_key_path.expect("file master key path"),
        )),
        SecretBackendChoice::Static(key) => Arc::new(StaticSecretKeyProvider::new(key)),
    };
    SecretCipher::new(provider)
}

#[cfg(test)]
mod atomicity_tests {
    use super::*;

    #[tokio::test]
    async fn failed_event_batch_is_invisible_and_can_be_retried_after_restart() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let harness =
            BasicExoHarness::new(crate::test_support::local_test_config(temp.path())).await?;
        let agent = harness
            .new_agent(NewAgentRequest {
                slug: "atomicity-test".to_string(),
                name: "Atomicity test".to_string(),
                vaults: vec![],
            })
            .await?;
        let thread = agent
            .new_conversation(NewConversationRequest::default())
            .await?;
        let agent_id = agent.record().id;
        let thread_id = thread.record().id;
        let committed_head = thread.record().latest_event_id;

        let batch = AddEventsRequest {
            session_id: None,
            turn_id: None,
            data: vec![
                EventData::Error {
                    message: "first".to_string(),
                    metadata: None,
                },
                EventData::Error {
                    message: "second".to_string(),
                    metadata: None,
                },
            ],
        };

        // The entire batch is one object write, so a failed write publishes no events.
        harness.inner.storage.fail_json_put_after(0);
        let error = thread
            .add_events(batch.clone())
            .await
            .expect_err("the batch object write should fail");
        assert!(
            error
                .to_string()
                .contains("injected JSON object write failure")
        );

        drop(thread);
        drop(agent);
        drop(harness);

        let reopened =
            BasicExoHarness::new(crate::test_support::local_test_config(temp.path())).await?;
        let agent = reopened
            .get_agent(&agent_id)
            .await?
            .expect("agent survives restart");
        let thread = agent
            .get_thread(&thread_id)
            .await?
            .expect("thread survives restart");
        let events = thread.get_events(None).await?.events;
        let error_messages = events
            .iter()
            .filter_map(|event| match &event.data {
                EventData::Error { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(error_messages.is_empty());
        assert_eq!(thread.record().latest_event_id, committed_head);
        assert_eq!(events.last().map(|event| event.id), committed_head);

        // Retrying publishes each event exactly once.
        let retry = thread.add_events(batch).await?;
        let thread = agent
            .get_thread(&thread_id)
            .await?
            .expect("thread survives retry");
        let error_messages = thread
            .get_events(None)
            .await?
            .events
            .into_iter()
            .filter_map(|event| match event.data {
                EventData::Error { message, .. } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(error_messages, ["first", "second"]);
        assert_eq!(thread.record().latest_event_id, Some(retry.latest_event_id));
        for id in retry.event_ids {
            assert!(thread.get_event(id).await?.is_some());
        }
        Ok(())
    }
}

#[cfg(test)]
mod snapshot_manifest_tests {
    use super::*;

    #[test]
    pub(super) fn snapshot_format_validation_uses_backend_capabilities() {
        let docker = crate::CliContainerSandboxBackend::docker();
        ensure_snapshot_format_supported(
            &docker,
            &SandboxProvider::Docker,
            &SnapshotFormat::DockerImageTar,
        )
        .expect("Docker should consume docker-image-tar");

        let error = ensure_snapshot_format_supported(
            &docker,
            &SandboxProvider::Docker,
            &SnapshotFormat::WorkspaceChunksV1,
        )
        .expect_err("Docker should reject undeclared formats");
        assert!(error.to_string().contains("workspace-chunks-v1"));
    }

    #[test]
    pub(super) fn stored_snapshot_manifest_reads_legacy_kind_field_and_value() {
        let snapshot_id = Uuid7::now();
        let manifest: StoredSnapshotManifest = serde_json::from_value(serde_json::json!({
            "snapshot_id": snapshot_id,
            "sandbox_id": "sandbox-legacy",
            "kind": "docker_image_tar",
            "created_at": "2026-08-29T00:00:00Z",
            "payload_size_bytes": 42,
        }))
        .expect("deserialize legacy snapshot manifest");
        assert_eq!(manifest.format, SnapshotFormat::DockerImageTar);

        let rewritten = serde_json::to_value(manifest).expect("serialize snapshot manifest");
        assert_eq!(rewritten.get("format").unwrap(), "docker-image-tar");
        assert!(rewritten.get("kind").is_none());
    }
}

impl BasicExoHarnessConfig {
    pub fn validate_secret_mount(&self, host_path: &Path) -> Result<()> {
        let host_path = host_path.canonicalize()?;
        let mut protected = vec![self.root.join("vaults")];
        if let Some(path) = self.secret_backend.master_key_path(&self.root) {
            protected.push(path);
        }
        for path in protected {
            let path = std::path::absolute(path)?;
            let mut ancestor = path.as_path();
            let mut canonical = loop {
                match ancestor.canonicalize() {
                    Ok(canonical) => break canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = ancestor
                            .parent()
                            .context("protected path has no existing ancestor")?;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            for component in path.strip_prefix(ancestor)?.components() {
                match component {
                    std::path::Component::ParentDir => {
                        canonical.pop();
                    }
                    component => canonical.push(component),
                }
            }
            if canonical.starts_with(&host_path) || host_path.starts_with(&canonical) {
                bail!(
                    "sandbox mount {} exposes vault storage or its master key",
                    host_path.display()
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod stored_policy_tests {
    use super::*;

    #[test]
    pub(super) fn ignores_retired_fields_in_stored_sandbox_credential_bindings() {
        let stored: StoredSandbox = serde_json::from_str(
            r#"{
                "id": "existing-native-sandbox", "provider": "docker", "image": "test",
                "file_system_mounts": [], "idle_seconds": 300, "running": true,
                "policy": {
                    "networking": {"type": "unrestricted"},
                    "credentials": [{
                        "name": "model:01900000-0000-7000-8000-000000000001",
                        "model": "01900000-0000-7000-8000-000000000001",
                        "environment_variable": "OPENAI_API_KEY",
                        "networking": {"type": "limited", "allowed_hosts": ["api.openai.com"]},
                        "injection_location": {"header": true}
                    }]
                }
            }"#,
        )
        .unwrap();
        let policy = stored.policy();
        assert_eq!(policy.credentials.len(), 1);
        assert_eq!(policy.credentials[0].environment_variable, "OPENAI_API_KEY");
        let serialized = serde_json::to_string(&stored).unwrap();
        assert!(!serialized.contains("\"model\":"));
        let current: StoredSandbox = serde_json::from_str(&serialized).unwrap();
        assert_eq!(current.policy().credentials, policy.credentials);
        assert!(
            serde_json::from_str::<StoredSandbox>(
                &serialized.replace("\"environment_variable\":", "\"misspelled_variable\":")
            )
            .is_err()
        );
    }

    #[test]
    pub(super) fn reads_legacy_networking_and_writes_only_the_policy() {
        let mut legacy: StoredSandbox = serde_json::from_value(serde_json::json!({
            "id": "legacy", "provider": "local_process", "image": "",
            "default_workdir": null, "file_system_mounts": [],
            "enable_networking": false, "idle_seconds": 60,
            "running": true, "latest_snapshot_id": null
        }))
        .unwrap();
        assert!(!legacy.policy().networking_enabled());
        legacy.network = StoredSandboxPolicy::Policy {
            policy: legacy.policy(),
        };
        let serialized = serde_json::to_string(&legacy).unwrap();
        assert!(!serialized.contains("enable_networking"));
        let current: StoredSandbox = serde_json::from_str(&serialized).unwrap();
        assert_eq!(current.policy(), legacy.policy());
        assert!(
            serde_json::from_value::<StoredSandbox>(serde_json::json!({
                "id": "invalid-policy", "provider": "local_process", "image": "",
                "default_workdir": null, "file_system_mounts": [],
                "enable_networking": true, "idle_seconds": 60,
                "running": true, "latest_snapshot_id": null,
                "policy": {"networking": {"type": "unsupported"}}
            }))
            .is_err()
        );
    }
}

#[cfg(all(test, feature = "firecracker"))]
mod egress_resolution_tests {
    use super::*;
    use crate::SandboxNetworkPolicy;

    #[tokio::test]
    pub(super) async fn policy_overrides_the_legacy_networking_flag() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut config = crate::test_support::local_test_config(directory.path());
        let limited: crate::EgressPolicy = SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        }
        .into();
        config.sandbox_policy = Some(limited.clone());
        let harness = BasicExoHarness::new(config).await?;
        let request = CreateSandboxRequest {
            tcp_ports: vec![],
            name: None,
            provider: SandboxProvider::LocalProcess,
            image: "".into(),
            resources: Default::default(),
            default_workdir: None,
            file_system_mounts: None,
            durable_file_systems: None,
            policy: None,
            enable_networking: Some(false),
            idle_seconds: None,
        };
        assert_eq!(
            prepare_sandbox_request(&harness, ResourceScope::Global, request.clone())
                .await?
                .policy,
            limited
        );
        assert_eq!(
            prepare_sandbox_request(
                &harness,
                ResourceScope::Global,
                CreateSandboxRequest {
                    policy: Some(SandboxNetworkPolicy::Disabled.into()),
                    enable_networking: Some(true),
                    ..request
                }
            )
            .await?
            .policy
            .networking,
            SandboxNetworkPolicy::Disabled
        );
        Ok(())
    }
}

pub(super) async fn remove_thread_resources(
    harness: &BasicExoHarness,
    agent: AgentId,
    thread: ConversationId,
) -> Result<()> {
    let store = harness.inner.native.resources.clone();
    if let Some(provider) = store.external_provider(agent, thread)? {
        harness
            .inner
            .sandbox_backend_for_provider(provider)
            .await?
            .remove_thread_resources(agent, thread)
            .await?;
    }
    tokio::task::spawn_blocking(move || store.remove_thread(agent, thread)).await?
}

impl BasicAgentHandle {
    pub(super) async fn prepare_resources_impl(
        &self,
        resources: Vec<crate::resources::ResourceDefinition>,
    ) -> Result<Vec<crate::resources::PreparedResource>> {
        self.harness
            .check(ResourceScope::Agent {
                agent_id: self.record.id,
            })
            .await?;
        if !resources.is_empty()
            && let Some(caller) = &self.harness.caller
        {
            caller.policy.check_operator(&caller.principal).await?;
        }
        let store = self.harness.inner.native.resources.clone();
        tokio::task::spawn_blocking(move || store.prepare(resources)).await?
    }
}

impl BasicConversationHandle {
    pub(super) async fn materialize_resources_impl(
        &self,
        resources: Vec<crate::resources::PreparedResource>,
        provider: SandboxProvider,
        lease: Option<Arc<sessions::SessionLease>>,
    ) -> Result<Vec<FileSystemMount>> {
        self.harness
            .check(ResourceScope::Thread {
                agent_id: self.agent_id,
                thread_id: self.record.id,
            })
            .await?;
        if resources.is_empty() {
            return Ok(Vec::new());
        }
        let store = self.harness.inner.native.resources.clone();
        let agent = self.agent_id;
        let thread = self.record.id;
        let resources_guard = self
            .harness
            .inner
            .lock_thread_resources(agent, thread)
            .await;
        let resume = store.has_thread(agent, thread);
        let external = provider == SandboxProvider::Firecracker;
        if resume {
            anyhow::ensure!(
                store.external_provider(agent, thread)?.is_some() == external,
                "cannot move thread resources between Firecracker and directory-based sandboxes"
            );
        }
        let backend = if external {
            Some(
                self.harness
                    .inner
                    .sandbox_backend_for_provider(provider)
                    .await?,
            )
        } else {
            None
        };
        let credentials = stream::iter(resources.iter().cloned().map(|resource| async move {
            let credential = if !resume
                && let crate::resources::ResourceSource::GitRepository {
                    url: Some(url),
                    credential,
                    ..
                } = &resource.definition.source
            {
                if let Some(name) = credential {
                    tracing::info!(target: "exoharness::progress", "Loading Git credentials");
                    let reference =
                        crate::vault::find_secret(self, name)
                            .await?
                            .with_context(|| {
                                format!(
                                    "Git resource credential {name} is not in the selected vaults"
                                )
                            })?;
                    let target = crate::vault::CredentialDestination::origin(
                        &url::Url::parse(url)?.origin().ascii_serialization(),
                    )?;
                    let vault = crate::vault::require_vault(self, &reference.vault_id).await?;
                    let resolved = vault.resolve_secret(&reference.secret_id, &target).await?;
                    let value = resolved.secret.bearer_value().to_owned();
                    Some(crate::resources::GitCredential {
                        identity: format!("{}:{}", reference.vault_id, reference.secret_id),
                        username: "x-access-token".into(),
                        token: value,
                    })
                } else if external && cfg!(target_os = "macos") {
                    let url = url.clone();
                    tokio::task::spawn_blocking(move || crate::resources::host_git_credential(&url))
                        .await??
                } else {
                    None
                }
            } else {
                None
            };
            Ok::<_, anyhow::Error>(credential)
        }))
        .buffered(8)
        .try_collect::<Vec<_>>()
        .await?;
        let harness = self.harness.clone();
        let record = self.conversation_dir().join("record.json");
        tokio::spawn(async move {
            // The task can outlive a cancelled request; retain ownership until
            // materialization finishes even if runtime shutdown has started.
            let _lease = lease;
            // Retain resource ownership in the detached task too, so deletion
            // cannot race a materializer whose caller has been cancelled.
            let _resources = resources_guard;
            {
                let _guard = harness.inner.write_lock.lock().await;
                harness
                    .inner
                    .storage
                    .get_json::<ConversationRecord>(record)
                    .await?;
            }
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let _lease = _lease;
                let Some(backend) = backend else {
                    return store.materialize(agent, thread, resources, credentials);
                };
                store.remember_external(agent, thread, None)?;
                let materialize = |sources| {
                    runtime.block_on(backend.materialize_resources(
                        crate::resources::MaterializeResourcesRequest {
                            agent,
                            thread,
                            resources: resources.clone(),
                            archives: sources,
                            credentials,
                            resume,
                        },
                    ))
                };
                let mounts = if resume {
                    materialize(Default::default())?
                } else {
                    store.with_image_sources(&resources, materialize)?
                };
                store.remember_external(agent, thread, Some(&resources))?;
                Ok(mounts)
            })
            .await?
        })
        .await?
    }
}

impl BasicConversationHandle {
    pub(super) fn check_resource_environment(&self, provider: &SandboxProvider) -> Result<()> {
        if self
            .harness
            .inner
            .native
            .resources
            .has_thread(self.agent_id, self.record.id)
        {
            anyhow::ensure!(
                self.harness
                    .inner
                    .native
                    .resources
                    .external_provider(self.agent_id, self.record.id)?
                    .is_some()
                    == (*provider == SandboxProvider::Firecracker),
                "cannot move a thread's existing resources between local and Firecracker storage"
            );
        }
        Ok(())
    }
}

impl BasicConversationHandle {
    pub(super) fn check_resource_fork(&self) -> Result<()> {
        anyhow::ensure!(
            !self
                .harness
                .inner
                .native
                .resources
                .has_thread(self.agent_id, self.record.id),
            "forking a thread with filesystem resources is not supported yet; create a new thread"
        );
        Ok(())
    }
}

pub struct TokioRuntimeHost;
impl crate::runtime_host::RuntimeHost for TokioRuntimeHost {
    fn spawn(&self, task: futures::future::BoxFuture<'static, ()>) {
        drop(tokio::spawn(task));
    }
}
