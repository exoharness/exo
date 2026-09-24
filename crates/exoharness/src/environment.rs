use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::CreateSandboxRequest;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub name: String,
    pub config: CreateSandboxRequest,
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
