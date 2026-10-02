use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::CreateSandboxRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub name: String,
    pub config: CreateSandboxRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previews: Option<BrowserPreviewConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BrowserPreviewConfig {
    pub domain: String,
    pub services: BTreeMap<String, u16>,
}

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
        if let Some(previews) = &self.previews {
            crate::types::canonical_egress_host(&previews.domain)?;
            ensure!(
                !previews.services.is_empty(),
                "previews.services must not be empty"
            );
            for (name, port) in &previews.services {
                ensure!(
                    !name.is_empty()
                        && name.len() <= 63
                        && !name.starts_with('-')
                        && !name.ends_with('-')
                        && name.bytes().all(|byte| byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || byte == b'-'),
                    "preview service names must be lowercase DNS labels"
                );
                ensure!(
                    *port > 0 && self.config.tcp_ports.contains(port),
                    "preview service {name} uses unpublished TCP port {port}"
                );
            }
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
    fn previews_require_an_explicit_domain_and_published_ports() -> Result<()> {
        let mut environment: EnvironmentDefinition = serde_json::from_str(
            r#"{"name":"dev","config":{"image":"dev","tcp_ports":[13000]},"previews":{"domain":"exo.localhost","services":{"app":13000}}}"#,
        )?;
        environment.validate()?;
        let previews = environment
            .previews
            .as_mut()
            .expect("preview configuration");
        previews.services.insert("api".into(), 8000);
        assert!(environment.validate().is_err());
        let previews = environment
            .previews
            .as_mut()
            .expect("preview configuration");
        previews.services.remove("api");
        previews.domain = "http://localhost".into();
        assert!(environment.validate().is_err());
        Ok(())
    }
}
