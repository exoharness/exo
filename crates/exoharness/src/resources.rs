use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::FileSystemMountMode;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceDefinition {
    pub name: String,
    pub mount_path: String,
    #[serde(default = "writable")]
    pub mode: FileSystemMountMode,
    #[serde(flatten)]
    pub source: ResourceSource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceSource {
    Directory {
        path: PathBuf,
    },
    GitRepository {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<GitCheckout>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitCheckout {
    Branch { name: String },
    Commit { sha: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedResource {
    pub definition: ResourceDefinition,
    /// Local sources are pinned at agent registration; remote Git is resolved per thread.
    pub snapshot: Option<String>,
}

fn writable() -> FileSystemMountMode {
    FileSystemMountMode::ReadWrite
}

impl ResourceDefinition {
    pub fn local_path(&self) -> Option<&Path> {
        match &self.source {
            ResourceSource::Directory { path } => Some(path),
            ResourceSource::GitRepository { path, .. } => path.as_deref(),
        }
    }

    pub fn resolve_path(&mut self, base: &Path) -> Result<()> {
        let path = match &mut self.source {
            ResourceSource::Directory { path } => Some(path),
            ResourceSource::GitRepository { path, .. } => path.as_mut(),
        };
        if let Some(path) = path {
            *path = base.join(&*path).canonicalize().with_context(|| {
                format!("resolving resource {} at {}", self.name, path.display())
            })?;
            ensure!(path.is_dir(), "resource {} must be a directory", self.name);
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.name.is_empty()
                && self
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
            "resource names may only contain letters, numbers, '-' and '_'"
        );
        let mount = Path::new(&self.mount_path);
        ensure!(
            mount.is_absolute()
                && mount != Path::new("/")
                && mount
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
                && !self.mount_path.contains(['\0', ':']),
            "resource mount_path must be an absolute path without '..', ':' or NUL"
        );
        match &self.source {
            ResourceSource::Directory { path } => ensure!(
                !path.as_os_str().is_empty(),
                "resource path must not be empty"
            ),
            ResourceSource::GitRepository {
                path,
                url,
                checkout,
                credential,
            } => {
                ensure!(
                    path.is_some() != url.is_some(),
                    "git_repository requires exactly one of path or url"
                );
                if let Some(path) = path {
                    ensure!(
                        !path.as_os_str().is_empty(),
                        "resource path must not be empty"
                    );
                    ensure!(
                        checkout.is_none() && credential.is_none(),
                        "local Git resources capture the current checkout; checkout and credential require a URL"
                    );
                }
                if let Some(url) = url {
                    let url = url::Url::parse(url).context("invalid Git resource URL")?;
                    ensure!(
                        url.scheme() == "https"
                            && url.host_str().is_some()
                            && url.username().is_empty()
                            && url.password().is_none()
                            && url.query().is_none()
                            && url.fragment().is_none(),
                        "Git resource URLs must use HTTPS without embedded credentials, query or fragment"
                    );
                }
                if let Some(checkout) = checkout {
                    match checkout {
                        GitCheckout::Branch { name } => ensure!(
                            !name.is_empty()
                                && !name.starts_with('-')
                                && !name.contains(['\0', '\n', '\r']),
                            "invalid Git branch"
                        ),
                        GitCheckout::Commit { sha } => ensure!(
                            matches!(sha.len(), 40 | 64)
                                && sha.bytes().all(|c| c.is_ascii_hexdigit()),
                            "Git commits must be full hexadecimal object IDs"
                        ),
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn validate_resources(resources: &[ResourceDefinition]) -> Result<()> {
    for (index, resource) in resources.iter().enumerate() {
        resource.validate()?;
        for other in &resources[..index] {
            ensure!(
                resource.name != other.name,
                "duplicate resource name: {}",
                resource.name
            );
            validate_mount_overlap(&resource.mount_path, &other.mount_path)?;
        }
    }
    Ok(())
}

pub fn validate_mount_overlap(left: &str, right: &str) -> Result<()> {
    ensure!(
        !Path::new(left).starts_with(right) && !Path::new(right).starts_with(left),
        "overlapping resource mounts: {left} and {right}"
    );
    Ok(())
}

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod local;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub(crate) use local::{ResourceStore, host_git_credential};

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
#[derive(Debug, Serialize, Deserialize)]
pub struct MaterializeResourcesRequest {
    pub agent: crate::AgentId,
    pub thread: crate::ThreadId,
    pub resources: Vec<PreparedResource>,
    pub archives: std::collections::BTreeMap<String, PathBuf>,
    pub credentials: Vec<Option<GitCredential>>,
    pub resume: bool,
}

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
#[derive(Serialize, Deserialize)]
pub struct GitCredential {
    pub identity: String,
    pub username: String,
    pub token: String,
}

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
impl std::fmt::Debug for GitCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCredential")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}
