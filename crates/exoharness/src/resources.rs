use std::{
    borrow::Cow,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::FileSystemMountMode;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResourceDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_path: Option<String>,
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
    #[serde(rename = "git")]
    GitRepository {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PreparedResource {
    pub definition: ResourceDefinition,
    /// Local sources are pinned at agent registration; remote Git is resolved per thread.
    pub snapshot: Option<String>,
}

impl PreparedResource {
    pub fn same_workspace(&self, other: &Self) -> bool {
        let mut definition = self.definition.clone();
        definition.mount_path = other.definition.mount_path.clone();
        if let (
            ResourceSource::GitRepository { credential, .. },
            ResourceSource::GitRepository {
                credential: other_credential,
                ..
            },
        ) = (&mut definition.source, &other.definition.source)
        {
            *credential = other_credential.clone();
        }
        self.snapshot == other.snapshot
            && self.definition.mount_path() == other.definition.mount_path()
            && definition == other.definition
    }
}

/// Parse a Git stderr progress record into a display phase and percentage.
pub fn git_progress(line: &[u8]) -> Option<(&'static str, u8)> {
    let line = std::str::from_utf8(line).ok()?.trim();
    let line = line.strip_prefix("remote: ").unwrap_or(line);
    let (phase, progress) = line.split_once(':')?;
    let phase = match phase {
        "Counting objects" => "Counting objects for",
        "Compressing objects" => "Compressing objects for",
        "Receiving objects" => "Downloading",
        "Resolving deltas" => "Resolving deltas for",
        "Updating files" => "Checking out",
        _ => return None,
    };
    let percent = progress.split_once('%')?.0.trim().parse::<u8>().ok()?;
    (percent <= 100).then_some((phase, percent))
}

fn writable() -> FileSystemMountMode {
    FileSystemMountMode::ReadWrite
}

impl ResourceDefinition {
    pub fn mount_path(&self) -> Cow<'_, str> {
        match &self.mount_path {
            Some(path) => Cow::Borrowed(path),
            None => Cow::Owned(format!("/workspace/{}", self.name)),
        }
    }

    pub fn git_credential_variable(&self) -> String {
        let name: String = self
            .name
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("EXO_GIT_{name}")
    }

    pub fn git_credential_helper(&self) -> String {
        let variable = self.git_credential_variable();
        format!(
            "!f() {{ if [ \"$1\" = get ] && [ -n \"${{{variable}:-}}\" ]; then printf 'username=x-access-token\\npassword=%s\\n' \"${variable}\"; fi; }}; f"
        )
    }

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
        ensure!(
            self.mount_path.is_some()
                || matches!(self.source, ResourceSource::GitRepository { .. }),
            "directory resources require mount_path"
        );
        let mount_path = self.mount_path();
        let mount = Path::new(mount_path.as_ref());
        ensure!(
            // Mount paths describe the sandbox filesystem, including on wasm
            // where std::Path::is_absolute rejects ordinary POSIX roots.
            mount.has_root()
                && mount != Path::new("/")
                && mount
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
                && !mount_path.contains(['\0', ':']),
            "resource mount_path must be an absolute path without '..', ':' or NUL: {:?}",
            mount_path
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
                    "git resources require exactly one of path or url"
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
                    ensure!(
                        !checkout.is_empty()
                            && !checkout.starts_with('-')
                            && !checkout
                                .chars()
                                .any(|c| c.is_control() || c.is_whitespace())
                            && !checkout.contains(['~', '^', ':', '?', '*', '[', '\\'])
                            && !checkout.contains("..")
                            && !checkout.contains("@{"),
                        "checkout must be a Git branch, tag, or full commit ID"
                    );
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
            validate_mount_overlap(&resource.mount_path(), &other.mount_path())?;
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
pub use local::ResourceStore;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub(crate) use local::host_git_credential;
#[derive(Debug, Serialize, Deserialize)]
pub struct MaterializeResourcesRequest {
    pub agent: crate::AgentId,
    pub thread: crate::ThreadId,
    pub resources: Vec<PreparedResource>,
    pub archives: std::collections::BTreeMap<String, PathBuf>,
    pub credentials: Vec<Option<GitCredential>>,
    pub resume: bool,
}

#[derive(Serialize, Deserialize)]
pub struct GitCredential {
    pub identity: String,
    pub username: String,
    pub token: String,
}

impl std::fmt::Debug for GitCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCredential")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// These backends mount resources prepared on the Exo host. Remote backends
/// populate Git workspaces through their ordinary sandbox process interface.
pub fn uses_host_resource_mounts(provider: &crate::SandboxProvider) -> bool {
    matches!(
        provider.as_str(),
        "apple_container" | "docker" | "local_process" | "firecracker" | "smolvm"
    )
}

pub fn validate_sandbox_resources(resources: &[PreparedResource]) -> Result<()> {
    for resource in resources {
        // Definitions were validated by prepare_resources; this checks only
        // the execution backend's mount capabilities after provider selection.
        ensure!(
            matches!(
                &resource.definition.source,
                ResourceSource::GitRepository {
                    path: None,
                    url: Some(_),
                    ..
                }
            ),
            "remote sandboxes require Git resource URLs; local directories cannot be mounted"
        );
        ensure!(
            resource.definition.mode == FileSystemMountMode::ReadWrite,
            "remote Git resources require read-write mode; read-only filesystem mounts are unavailable"
        );
    }
    Ok(())
}
