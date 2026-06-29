// Policy sandbox: a per-agent sandbox dedicated to the agent's own policy/source
// (distinct from the agent/conversation env sandbox where `shell` runs). The
// `policy_shell` tool runs commands here so the agent can inspect, edit, and
// build its own code in an isolated container. v1 reuses the conversation's
// sandbox spec but with a separate name, so it is a distinct container that
// still sees the same repo mount. The spec can diverge later (dedicated
// toolchain image, copy-not-mount, executor entrypoint) without changing this
// resolver's shape — this mirrors agent_sandbox.rs intentionally.

use exoharness::{
    AgentHandle, Artifact, ConversationHandle, CreateSandboxRequest, DurableFileSystem,
    FileSystemMountMode, ReadArtifactRequest, Result, SandboxProvider, SnapshotId,
    StartSandboxRequest, Uuid7, WriteArtifactRequest,
};
use serde::{Deserialize, Serialize};

use crate::conversation_sandbox::{ConversationSandboxSpec, conversation_sandbox_spec};
use crate::{AgentConfig, ConversationConfig};

const POLICY_SANDBOX_ARTIFACT_PATH: &str = "config/policy-sandbox.json";
const POLICY_SANDBOX_NAME_PREFIX: &str = "policy-sandbox";
// Warm sandboxes are reused by spec hash, not by name, so the policy sandbox
// must have a spec that differs from the env sandbox or it gets de-duped onto
// the same container. This marker durable filesystem guarantees a distinct
// spec hash regardless of the conversation's mounts, and doubles as a
// persistent scratch volume for the policy box.
const POLICY_MARKER_FS_NAME: &str = "exoclaw-policy";
const POLICY_MARKER_FS_PATH: &str = "/policy";

// The policy sandbox reuses the conversation's spec (same image + repo mount)
// but adds the marker durable filesystem so it resolves to its own warm
// container. Keep this the single source of truth for the policy spec so the
// create path and the matches_spec reuse check stay in sync.
pub(crate) fn policy_sandbox_spec(
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> ConversationSandboxSpec {
    let mut spec = conversation_sandbox_spec(agent_config, conversation_config);
    spec.durable_file_systems.push(DurableFileSystem {
        name: POLICY_MARKER_FS_NAME.to_string(),
        mount_path: POLICY_MARKER_FS_PATH.to_string(),
        mode: FileSystemMountMode::ReadWrite,
    });
    spec
}

#[derive(Clone)]
pub(crate) struct PolicySandboxHandle {
    pub(crate) conversation: std::sync::Arc<dyn ConversationHandle>,
    pub(crate) sandbox_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicySandboxRecord {
    conversation_id: String,
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

impl PolicySandboxRecord {
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

/// Snapshot the agent's policy sandbox and return the snapshot id. Host-side
/// entry point (no turn handle) so a supervisor outside the sandbox can record a
/// known-good baseline. The policy sandbox is agent-owned, so this mirrors the
/// `Agent` snapshot path (`agent.snapshot_sandbox`).
pub async fn snapshot_policy_sandbox(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> Result<SnapshotId> {
    let handle = ensure_policy_sandbox(agent, conversation, agent_config, conversation_config).await?;
    agent.snapshot_sandbox(handle.sandbox_id).await
}

/// Rewind the agent's policy sandbox to a snapshot. Host-side entry point so the
/// supervisor can roll back from *outside* the sandbox after a bad self-change —
/// the in-sandbox executor can't rewind the container it runs in.
pub async fn rewind_policy_sandbox(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
    snapshot_id: SnapshotId,
) -> Result<()> {
    let spec = policy_sandbox_spec(agent_config, conversation_config);
    let handle = ensure_policy_sandbox(agent, conversation, agent_config, conversation_config).await?;
    agent
        .start_sandbox(StartSandboxRequest {
            id: handle.sandbox_id,
            snapshot_id,
            idle_seconds: Some(spec.idle_seconds),
        })
        .await
}

pub(crate) async fn ensure_policy_sandbox(
    agent: &dyn AgentHandle,
    current_conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> Result<PolicySandboxHandle> {
    let spec = policy_sandbox_spec(agent_config, conversation_config);
    if let Some(handle) = current_policy_sandbox(agent, &spec).await? {
        return Ok(handle);
    }

    let sandbox_name = new_policy_sandbox_name();
    let sandbox_id = current_conversation
        .create_sandbox(CreateSandboxRequest {
            name: Some(sandbox_name.clone()),
            provider: spec.provider,
            image: spec.image.clone(),
            default_workdir: Some(spec.default_workdir.clone()),
            file_system_mounts: Some(spec.file_system_mounts.clone()),
            durable_file_systems: Some(spec.durable_file_systems.clone()),
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await?;
    store_policy_sandbox_record(
        agent,
        &PolicySandboxRecord {
            conversation_id: current_conversation.record().id.to_string(),
            sandbox_name,
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
            "policy sandbox owner conversation disappeared: {}",
            current_conversation.record().id
        );
    };
    Ok(PolicySandboxHandle {
        conversation: owner,
        sandbox_id,
    })
}

pub(crate) async fn current_policy_sandbox(
    agent: &dyn AgentHandle,
    spec: &ConversationSandboxSpec,
) -> Result<Option<PolicySandboxHandle>> {
    let Some(record) = load_policy_sandbox_record(agent).await? else {
        return Ok(None);
    };
    if !record.matches_spec(spec) {
        return Ok(None);
    }
    let Ok(conversation_id) = record.conversation_id.parse::<Uuid7>() else {
        return Ok(None);
    };
    let Some(owner) = agent.get_conversation(&conversation_id).await? else {
        return Ok(None);
    };
    let sandbox_id = owner
        .create_sandbox(CreateSandboxRequest {
            name: Some(record.sandbox_name),
            provider: spec.provider,
            image: spec.image.clone(),
            default_workdir: Some(spec.default_workdir.clone()),
            file_system_mounts: Some(spec.file_system_mounts.clone()),
            durable_file_systems: Some(spec.durable_file_systems.clone()),
            enable_networking: Some(spec.enable_networking),
            idle_seconds: Some(spec.idle_seconds),
        })
        .await?;
    Ok(Some(PolicySandboxHandle {
        conversation: owner,
        sandbox_id,
    }))
}

async fn load_policy_sandbox_record(
    agent: &dyn AgentHandle,
) -> Result<Option<PolicySandboxRecord>> {
    let Some(artifact) = latest_policy_artifact(agent, POLICY_SANDBOX_ARTIFACT_PATH).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&artifact.contents)?))
}

// Reads the newest version of an agent artifact by path. Mirrors the helper in
// agent_sandbox.rs (kept private there); resolves the latest version via
// list_artifacts before read_artifact, since ReadArtifactRequest is by id.
async fn latest_policy_artifact(agent: &dyn AgentHandle, path: &str) -> Result<Option<Artifact>> {
    let Some(version) = agent
        .list_artifacts()
        .await?
        .into_iter()
        .filter(|artifact| artifact.path == path)
        .max_by_key(|artifact| artifact.version)
    else {
        return Ok(None);
    };
    agent
        .read_artifact(ReadArtifactRequest {
            artifact_id: version.artifact_id,
            version: Some(version.version),
        })
        .await
}

async fn store_policy_sandbox_record(
    agent: &dyn AgentHandle,
    record: &PolicySandboxRecord,
) -> Result<()> {
    agent
        .write_artifact(WriteArtifactRequest {
            path: POLICY_SANDBOX_ARTIFACT_PATH.to_string(),
            contents: serde_json::to_vec_pretty(record)?,
        })
        .await?;
    Ok(())
}

fn new_policy_sandbox_name() -> String {
    format!("{POLICY_SANDBOX_NAME_PREFIX}-{}", Uuid7::now())
}
