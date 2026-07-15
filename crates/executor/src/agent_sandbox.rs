use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use exoharness::{
    AgentHandle, Artifact, ArtifactVersion, CreateSandboxRequest, ReadArtifactRequest, Result,
    SandboxProvider, Uuid7, WriteArtifactRequest,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use crate::conversation_sandbox::{ConversationSandboxSpec, conversation_sandbox_spec};
use crate::{AgentConfig, ConversationConfig};

// v1 was a conversation-owned handle, v2 a single agent-owned record whose
// one slot let conversations with different specs evict each other's sandbox.
// v3 keeps one record per spec; earlier paths are ignored, so pre-v3 sandboxes
// are abandoned and recreated per spec on first use.
const AGENT_SANDBOXES_ARTIFACT_PATH: &str = "config/agent-sandboxes-v3.json";
const AGENT_SANDBOX_NAME_PREFIX: &str = "agent-sandbox";

#[derive(Clone)]
pub(crate) struct AgentSandboxHandle {
    pub(crate) sandbox_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSandboxRecords {
    sandboxes: Vec<AgentSandboxRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSandboxRecord {
    sandbox_name: String,
    provider: SandboxProvider,
    image: String,
    default_workdir: String,
    file_system_mounts: Vec<exoharness::FileSystemMount>,
    #[serde(default)]
    durable_file_systems: Vec<exoharness::DurableFileSystem>,
    enable_networking: bool,
    idle_seconds: u64,
}

impl AgentSandboxRecord {
    fn matches_spec(&self, spec: &ConversationSandboxSpec) -> bool {
        self.provider == spec.provider
            && self.image == spec.image
            && self.default_workdir == spec.default_workdir
            && self.file_system_mounts == spec.file_system_mounts
            && self.durable_file_systems == spec.durable_file_systems
            && self.enable_networking == spec.enable_networking
            && self.idle_seconds == spec.idle_seconds
    }
}

pub(crate) async fn ensure_agent_sandbox(
    agent: &dyn AgentHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> Result<AgentSandboxHandle> {
    let sandbox_lock = agent_sandbox_lock(&agent.record().id.to_string());
    let _guard = sandbox_lock.lock().await;
    let spec = conversation_sandbox_spec(agent_config, conversation_config);
    let mut records = load_agent_sandbox_records(agent).await?;
    if let Some(record) = records.iter().find(|record| record.matches_spec(&spec)) {
        return attach_agent_sandbox(agent, record.sandbox_name.clone(), &spec).await;
    }

    let sandbox_name = new_agent_sandbox_name();
    let handle = attach_agent_sandbox(agent, sandbox_name.clone(), &spec).await?;
    records.push(agent_sandbox_record(sandbox_name, &spec));
    store_agent_sandbox_records(agent, &records).await?;
    Ok(handle)
}

pub(crate) async fn current_agent_sandbox(
    agent: &dyn AgentHandle,
    spec: &ConversationSandboxSpec,
) -> Result<Option<AgentSandboxHandle>> {
    let records = load_agent_sandbox_records(agent).await?;
    let Some(record) = records.iter().find(|record| record.matches_spec(spec)) else {
        return Ok(None);
    };
    Ok(Some(
        attach_agent_sandbox(agent, record.sandbox_name.clone(), spec).await?,
    ))
}

async fn attach_agent_sandbox(
    agent: &dyn AgentHandle,
    sandbox_name: String,
    spec: &ConversationSandboxSpec,
) -> Result<AgentSandboxHandle> {
    let sandbox_id = agent
        .create_sandbox(CreateSandboxRequest {
            name: Some(sandbox_name),
            provider: spec.provider,
            image: spec.image.clone(),
            default_workdir: Some(spec.default_workdir.clone()),
            file_system_mounts: Some(spec.file_system_mounts.clone()),
            durable_file_systems: Some(spec.durable_file_systems.clone()),
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await?;
    Ok(AgentSandboxHandle { sandbox_id })
}

fn agent_sandbox_record(
    sandbox_name: String,
    spec: &ConversationSandboxSpec,
) -> AgentSandboxRecord {
    AgentSandboxRecord {
        sandbox_name,
        provider: spec.provider,
        image: spec.image.clone(),
        default_workdir: spec.default_workdir.clone(),
        file_system_mounts: spec.file_system_mounts.clone(),
        durable_file_systems: spec.durable_file_systems.clone(),
        enable_networking: spec.enable_networking,
        idle_seconds: spec.idle_seconds,
    }
}

async fn load_agent_sandbox_records(agent: &dyn AgentHandle) -> Result<Vec<AgentSandboxRecord>> {
    let Some(artifact) = latest_agent_artifact(agent, AGENT_SANDBOXES_ARTIFACT_PATH).await? else {
        return Ok(Vec::new());
    };
    let records: AgentSandboxRecords = serde_json::from_slice(&artifact.contents)?;
    Ok(records.sandboxes)
}

async fn store_agent_sandbox_records(
    agent: &dyn AgentHandle,
    records: &[AgentSandboxRecord],
) -> Result<()> {
    agent
        .write_artifact(WriteArtifactRequest {
            path: AGENT_SANDBOXES_ARTIFACT_PATH.to_string(),
            contents: serde_json::to_vec_pretty(&AgentSandboxRecords {
                sandboxes: records.to_vec(),
            })?,
        })
        .await?;
    Ok(())
}

async fn latest_agent_artifact(agent: &dyn AgentHandle, path: &str) -> Result<Option<Artifact>> {
    let Some(version) = latest_artifact_version(agent.list_artifacts().await?, path) else {
        return Ok(None);
    };
    agent
        .read_artifact(ReadArtifactRequest {
            artifact_id: version.artifact_id,
            version: Some(version.version),
        })
        .await
}

fn latest_artifact_version(artifacts: Vec<ArtifactVersion>, path: &str) -> Option<ArtifactVersion> {
    artifacts
        .into_iter()
        .filter(|artifact| artifact.path == path)
        .max_by_key(|artifact| artifact.version)
}

fn new_agent_sandbox_name() -> String {
    format!("{AGENT_SANDBOX_NAME_PREFIX}-{}", Uuid7::now())
}

fn agent_sandbox_lock(agent_id: &str) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().expect("agent sandbox lock registry poisoned");
    Arc::clone(
        locks
            .entry(agent_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
    )
}

#[cfg(test)]
mod tests {
    use exoharness::{BasicExoHarness, ExoHarness, NewAgentRequest};
    use tempfile::TempDir;

    use super::*;
    use crate::AgentHarnessKind;
    use crate::test_support::local_test_config;

    async fn test_agent(tempdir: &TempDir) -> std::sync::Arc<dyn AgentHandle> {
        let exoharness = BasicExoHarness::new(local_test_config(tempdir.path().join("exoharness")))
            .await
            .unwrap();
        exoharness
            .new_agent(NewAgentRequest {
                slug: "agent".to_string(),
                name: "Agent".to_string(),
            })
            .await
            .unwrap()
    }

    fn test_agent_config() -> AgentConfig {
        AgentConfig {
            instructions: vec![],
            harness: AgentHarnessKind::Basic,
            typescript: None,
            enable_agent_tool_creation: true,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            enable_networking: false,
            model: "test-model".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        }
    }

    fn conversation_config(image: &str) -> ConversationConfig {
        ConversationConfig {
            sandbox_image: Some(image.to_string()),
            ..ConversationConfig::default()
        }
    }

    #[tokio::test]
    async fn keeps_one_stable_sandbox_per_spec() {
        let tempdir = TempDir::new().unwrap();
        let agent = test_agent(&tempdir).await;
        let agent_config = test_agent_config();
        let config_a = conversation_config("image-a");
        let config_b = conversation_config("image-b");

        let a1 = ensure_agent_sandbox(agent.as_ref(), &agent_config, &config_a)
            .await
            .unwrap();
        let b1 = ensure_agent_sandbox(agent.as_ref(), &agent_config, &config_b)
            .await
            .unwrap();
        // Alternating specs previously evicted each other's record; each spec
        // must keep reattaching to its own sandbox.
        let a2 = ensure_agent_sandbox(agent.as_ref(), &agent_config, &config_a)
            .await
            .unwrap();
        let b2 = ensure_agent_sandbox(agent.as_ref(), &agent_config, &config_b)
            .await
            .unwrap();

        assert_ne!(a1.sandbox_id, b1.sandbox_id);
        assert_eq!(a1.sandbox_id, a2.sandbox_id);
        assert_eq!(b1.sandbox_id, b2.sandbox_id);
    }

    #[tokio::test]
    async fn current_agent_sandbox_only_reports_matching_specs() {
        let tempdir = TempDir::new().unwrap();
        let agent = test_agent(&tempdir).await;
        let agent_config = test_agent_config();
        let config = conversation_config("image-a");

        let spec_a = conversation_sandbox_spec(&agent_config, &config);
        assert!(
            current_agent_sandbox(agent.as_ref(), &spec_a)
                .await
                .unwrap()
                .is_none()
        );

        let created = ensure_agent_sandbox(agent.as_ref(), &agent_config, &config)
            .await
            .unwrap();
        let current = current_agent_sandbox(agent.as_ref(), &spec_a)
            .await
            .unwrap()
            .expect("sandbox should be recorded for its spec");
        assert_eq!(current.sandbox_id, created.sandbox_id);

        let spec_b = conversation_sandbox_spec(&agent_config, &conversation_config("image-b"));
        assert!(
            current_agent_sandbox(agent.as_ref(), &spec_b)
                .await
                .unwrap()
                .is_none()
        );
    }
}
