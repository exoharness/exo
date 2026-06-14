use exoharness::{
    AgentHandle, Artifact, ArtifactVersion, ConversationHandle, CreateSandboxRequest,
    ReadArtifactRequest, Result, SandboxProvider, Uuid7, WriteArtifactRequest,
};
use serde::{Deserialize, Serialize};

use crate::conversation_sandbox::{
    ConversationSandboxSpec, conversation_sandbox_spec, conversation_sandboxes,
};
use crate::{AgentConfig, ConversationConfig};

const AGENT_SANDBOX_ARTIFACT_PATH: &str = "config/exoclaw-agent-sandbox.json";

#[derive(Clone)]
pub(crate) struct AgentSandboxHandle {
    pub(crate) conversation: std::sync::Arc<dyn ConversationHandle>,
    pub(crate) sandbox_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentSandboxRecord {
    conversation_id: String,
    sandbox_id: String,
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
    current_conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> Result<AgentSandboxHandle> {
    let spec = conversation_sandbox_spec(agent_config, conversation_config);
    if let Some(record) = load_agent_sandbox_record(agent).await?
        && record.matches_spec(&spec)
        && let Ok(conversation_id) = record.conversation_id.parse::<Uuid7>()
        && let Some(owner) = agent.get_conversation(&conversation_id).await?
    {
        for sandbox in conversation_sandboxes(owner.as_ref()).await? {
            if sandbox.id == record.sandbox_id && sandbox.matches_spec(&spec) {
                return Ok(AgentSandboxHandle {
                    conversation: owner,
                    sandbox_id: record.sandbox_id,
                });
            }
        }
    }

    let sandbox_id = current_conversation
        .create_sandbox(CreateSandboxRequest {
            name: None,
            provider: spec.provider,
            image: spec.image.clone(),
            default_workdir: Some(spec.default_workdir.clone()),
            file_system_mounts: Some(spec.file_system_mounts.clone()),
            durable_file_systems: Some(spec.durable_file_systems.clone()),
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await?;
    store_agent_sandbox_record(
        agent,
        &AgentSandboxRecord {
            conversation_id: current_conversation.record().id.to_string(),
            sandbox_id: sandbox_id.clone(),
            provider: spec.provider,
            image: spec.image,
            default_workdir: spec.default_workdir,
            file_system_mounts: spec.file_system_mounts,
            durable_file_systems: spec.durable_file_systems,
            enable_networking: spec.enable_networking,
            idle_seconds: spec.idle_seconds,
        },
    )
    .await?;

    let Some(owner) = agent
        .get_conversation(&current_conversation.record().id)
        .await?
    else {
        anyhow::bail!(
            "agent sandbox owner conversation disappeared: {}",
            current_conversation.record().id
        );
    };
    Ok(AgentSandboxHandle {
        conversation: owner,
        sandbox_id,
    })
}

async fn load_agent_sandbox_record(agent: &dyn AgentHandle) -> Result<Option<AgentSandboxRecord>> {
    let Some(artifact) = latest_agent_artifact(agent, AGENT_SANDBOX_ARTIFACT_PATH).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&artifact.contents)?))
}

async fn store_agent_sandbox_record(
    agent: &dyn AgentHandle,
    record: &AgentSandboxRecord,
) -> Result<()> {
    agent
        .write_artifact(WriteArtifactRequest {
            path: AGENT_SANDBOX_ARTIFACT_PATH.to_string(),
            contents: serde_json::to_vec_pretty(record)?,
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
