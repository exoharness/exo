use anyhow::Result;
use exo_managed_agents::AgentDefinition;
use exoharness::{AgentHandle, AgentId, ArtifactVersion};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(super) struct DefinitionCache {
    entries: Mutex<BTreeMap<AgentId, (ArtifactVersion, Arc<AgentDefinition>)>>,
}

impl DefinitionCache {
    pub fn retain(&self, agents: &BTreeSet<AgentId>) {
        self.entries
            .lock()
            .expect("agent definitions poisoned")
            .retain(|id, _| agents.contains(id));
    }
    pub async fn load(&self, agent: &dyn AgentHandle) -> Result<Option<Arc<AgentDefinition>>> {
        let latest = exo_managed_agents::definition_version(agent).await?;
        let Some(latest) = latest else {
            self.entries
                .lock()
                .expect("agent definitions poisoned")
                .remove(&agent.record().id);
            return Ok(None);
        };
        if let Some((version, definition)) = self
            .entries
            .lock()
            .expect("agent definitions poisoned")
            .get(&agent.record().id)
            && version.artifact_id == latest.artifact_id
            && version.version == latest.version
        {
            return Ok(Some(definition.clone()));
        }
        let Some(definition) = exo_managed_agents::load_definition_version(agent, &latest).await?
        else {
            return Ok(None);
        };
        let definition = Arc::new(definition);
        self.entries
            .lock()
            .expect("agent definitions poisoned")
            .insert(agent.record().id, (latest, definition.clone()));
        Ok(Some(definition))
    }
}
