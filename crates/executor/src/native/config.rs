use crate::AgentConfig;
use crate::managed_agents::{HarnessModules, TypeScriptHarnessPreset, agent_config_with_modules};
use anyhow::Result;
use exo_managed_agents::AgentDefinition;
use exoharness::SandboxProvider;
use std::path::{Path, PathBuf};

pub(crate) fn installation() -> Result<PathBuf> {
    crate::typescript::typescript_workspace_root()
}

struct NativeHarnessModules;

impl HarnessModules for NativeHarnessModules {
    fn installation(&self) -> Result<PathBuf> {
        installation()
    }
    fn resolve(&self, path: &Path) -> Result<PathBuf> {
        Ok(path.canonicalize()?)
    }
    fn preset_image(&self, preset: TypeScriptHarnessPreset) -> Option<String> {
        preset.sandbox_image().map(str::to_owned)
    }
}

pub fn agent_config(
    definition: &AgentDefinition,
    sandbox: SandboxProvider,
    harness: Option<&str>,
    model: Option<&str>,
) -> Result<AgentConfig> {
    agent_config_with_modules(definition, sandbox, harness, model, &NativeHarnessModules)
}
