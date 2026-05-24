use anyhow::{Result, bail};
use serde::Serialize;

use super::types::{AdapterConfig, AdapterKind, AdapterRecord, AdapterSource};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AdapterDefinition {
    pub source: AdapterSource,
    pub kind: AdapterKind,
    pub name: String,
    pub capabilities: Vec<String>,
}

pub fn adapter_definition(adapter: &AdapterRecord) -> AdapterDefinition {
    let capabilities = match &adapter.config {
        AdapterConfig::Worker(config) => config.capabilities.clone(),
        AdapterConfig::Module(config) => config.capabilities.clone(),
    };
    AdapterDefinition {
        source: adapter.source,
        kind: adapter.kind,
        name: adapter.name.clone(),
        capabilities,
    }
}

pub fn validate_adapter_build(adapter: &AdapterRecord) -> Result<()> {
    match adapter.source {
        AdapterSource::BuiltIn => Ok(()),
        AdapterSource::Library | AdapterSource::Agent => match &adapter.config {
            AdapterConfig::Module(config) => {
                if config.capabilities.is_empty() {
                    bail!("module adapter must declare at least one capability");
                }
                Ok(())
            }
            AdapterConfig::Worker(_) => Ok(()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{AdapterSource, ModuleAdapterConfig, NewAdapter};

    use super::*;

    #[test]
    fn rejects_module_adapters_without_capabilities() {
        let adapter = AdapterRecord::new(
            NewAdapter {
                agent_id: "agent".to_string(),
                conversation_id: "conversation".to_string(),
                name: "custom".to_string(),
                source: AdapterSource::Agent,
                config: AdapterConfig::Module(ModuleAdapterConfig {
                    module_path: "./adapter.ts".to_string(),
                    initialization: serde_json::json!({}),
                    capabilities: Vec::new(),
                }),
            },
            1,
        )
        .unwrap();

        assert!(validate_adapter_build(&adapter).is_err());
    }
}
