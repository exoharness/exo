use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::CreateSandboxRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub name: String,
    pub config: CreateSandboxRequest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previews: Option<BrowserPreviewConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrowserPreviewConfig {}

impl EnvironmentDefinition {
    pub fn validate_name(name: &str) -> Result<()> {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "environment name must contain 1–128 letters, digits, dashes, or underscores"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        Self::validate_name(&self.name)?;
        if self.previews.is_some() {
            ensure!(
                !self.config.tcp_ports.is_empty()
                    && self.config.tcp_ports.iter().all(|port| *port > 0),
                "previews require nonzero published TCP ports"
            );
        }
        ensure!(
            !self.config.image.trim().is_empty(),
            "environment image must not be empty"
        );
        if let Some(workdir) = &self.config.default_workdir {
            ensure!(
                workdir.starts_with('/'),
                "environment default_workdir must be absolute"
            );
        }
        if let Some(mounts) = &self.config.file_system_mounts {
            for (index, mount) in mounts.iter().enumerate() {
                ensure!(
                    std::path::Path::new(&mount.host_path).is_absolute(),
                    "environment mount host paths must be absolute paths on the runtime host"
                );
                ensure!(
                    mount.mount_path.starts_with('/'),
                    "environment mount paths must be absolute"
                );
                ensure!(
                    !mounts[..index]
                        .iter()
                        .any(|other| other.mount_path == mount.mount_path),
                    "duplicate environment mount: {}",
                    mount.mount_path
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxProvider;

    #[test]
    fn omitted_environment_provider_defaults_to_smolvm() -> Result<()> {
        let environment: EnvironmentDefinition = serde_json::from_str(
            r#"{"name":"coding","config":{"image":"ghcr.io/exoharness/codex-sandbox:latest"}}"#,
        )?;
        assert_eq!(environment.config.provider, SandboxProvider::Smolvm);
        environment.validate()?;
        Ok(())
    }

    #[test]
    fn previews_require_published_ports() -> Result<()> {
        let mut environment: EnvironmentDefinition = serde_json::from_str(
            r#"{"name":"dev","config":{"image":"dev","tcp_ports":[13000]},"previews":{}}"#,
        )?;
        environment.validate()?;
        environment.config.tcp_ports.clear();
        assert!(environment.validate().is_err());
        environment.config.tcp_ports.push(0);
        assert!(environment.validate().is_err());
        Ok(())
    }
}
