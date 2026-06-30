// Policy sandbox: a per-agent sandbox dedicated to the agent's own policy/source
// (distinct from the agent/conversation env sandbox where `shell` runs). The
// `policy_shell` tool runs commands here so the agent can inspect, edit, and
// build its own code in an isolated container; policy self-evolution snapshots
// and rewinds it. It reuses the conversation's sandbox spec (same image + repo
// mount) but adds a marker durable filesystem so it resolves to its OWN warm
// container (warm sandboxes are reused by spec hash, not by name).
//
// This mirrors agent_sandbox.rs: the sandbox is AGENT-owned (agent.create_sandbox
// / run_in_sandbox / snapshot_sandbox / start_sandbox), not conversation-owned.

use std::collections::HashMap;

use exoharness::{
    AgentHandle, Artifact, CreateSandboxRequest, DurableFileSystem, FileSystemMountMode,
    ReadArtifactRequest, Result, RunInSandboxRequest, SandboxProcess, SandboxProvider, SnapshotId,
    StartSandboxRequest, Uuid7, WriteArtifactRequest,
};
use futures::io::AsyncReadExt;
use serde::{Deserialize, Serialize};

use crate::conversation_sandbox::{ConversationSandboxSpec, conversation_sandbox_spec};
use crate::{AgentConfig, ConversationConfig};

const POLICY_SANDBOX_ARTIFACT_PATH: &str = "config/policy-sandbox-v2.json";
const POLICY_SANDBOX_NAME_PREFIX: &str = "policy-sandbox";
// Warm sandboxes are reused by spec hash, not by name, so the policy sandbox
// must have a spec that differs from the env sandbox or it gets de-duped onto
// the same container. This marker durable filesystem guarantees a distinct spec
// hash regardless of the conversation's mounts, and doubles as a persistent
// scratch volume for the policy box.
const POLICY_MARKER_FS_NAME: &str = "exoclaw-policy";
const POLICY_MARKER_FS_PATH: &str = "/policy";
// Absolute path of the prebuilt exo binary inside the policy sandbox image (see
// examples/exoclaw/policy-sandbox/Dockerfile). It is not on PATH, so the
// host-driven policy repl invokes it by full path -- matching the supervisor's
// CONTAINER_EXO default.
const POLICY_CONTAINER_EXO_BIN: &str = "/home/worker/exo/target/debug/exo";

#[derive(Clone)]
pub(crate) struct PolicySandboxHandle {
    pub(crate) sandbox_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PolicySandboxRecord {
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

// The policy sandbox reuses the conversation's spec (same image + repo mount)
// but adds the marker durable filesystem so it resolves to its own warm
// container. Single source of truth for the policy spec so the create path and
// the matches_spec reuse check stay in sync.
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

/// Snapshot the agent's policy sandbox and return the snapshot id. Host-side
/// entry point (no turn handle) so a supervisor outside the sandbox can record a
/// known-good baseline.
pub async fn snapshot_policy_sandbox(
    agent: &dyn AgentHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> Result<SnapshotId> {
    let handle = ensure_policy_sandbox(agent, agent_config, conversation_config).await?;
    agent.snapshot_sandbox(handle.sandbox_id).await
}

/// Rewind the agent's policy sandbox to a snapshot. Host-side entry point so the
/// supervisor can roll back from *outside* the sandbox after a bad self-change.
pub async fn rewind_policy_sandbox(
    agent: &dyn AgentHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
    snapshot_id: SnapshotId,
) -> Result<()> {
    let spec = policy_sandbox_spec(agent_config, conversation_config);
    let handle = ensure_policy_sandbox(agent, agent_config, conversation_config).await?;
    agent
        .start_sandbox(StartSandboxRequest {
            id: handle.sandbox_id,
            snapshot_id,
            idle_seconds: Some(spec.idle_seconds),
        })
        .await
}

/// Run one conversation turn *inside* the agent's policy sandbox and return the
/// reply text. This is the host-driven "policy repl" primitive: the loop lives
/// on the host (so it survives a rollback that recreates the container), while
/// the turn itself executes against the policy sandbox's own (possibly evolved)
/// `harness.ts`.
///
/// It dispatches by the stable exo sandbox id (not a docker container id), so
/// the kernel transparently lands the turn in the current container even after
/// a rewind or idle-eviction recreated it.
///
/// `eh_url`/`bearer_env`/`bearer_value` describe how the *in-sandbox* exo reaches
/// the kernel over HTTP; they mirror the host's own `--exoharness-url` /
/// `--bearer-env` so the in-sandbox executor is a pure remote client.
#[allow(clippy::too_many_arguments)]
pub async fn policy_repl_turn(
    agent: &dyn AgentHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
    eh_url: &str,
    bearer_env: Option<&str>,
    bearer_value: Option<&str>,
    agent_slug: &str,
    conversation_slug: &str,
    line: &str,
) -> Result<String> {
    let handle = ensure_policy_sandbox(agent, agent_config, conversation_config).await?;

    let mut env = HashMap::new();
    // Pure remote client: the kernel (not the sandbox) owns all sandboxes.
    env.insert("EXO_REMOTE_SANDBOX".to_string(), "1".to_string());
    // The user's message is passed via the environment (not interpolated into
    // the shell line) so it cannot break out of the command.
    env.insert("EXO_POLICY_REPL_LINE".to_string(), line.to_string());

    let bearer_flag = match bearer_env {
        Some(name) => {
            // The in-sandbox exo reads the token from this env var by name.
            env.insert(name.to_string(), bearer_value.unwrap_or("").to_string());
            format!("--bearer-env {name} ")
        }
        None => String::new(),
    };

    // run_in_sandbox has no workdir field and the policy sandbox's default
    // workdir is not the repo root, so cd there first (the in-sandbox exo
    // resolves the TS harness runner relative to cwd). agent/conversation slugs
    // and the kernel URL are operator-controlled; only the message is untrusted,
    // and it travels via $EXO_POLICY_REPL_LINE.
    let script = format!(
        "cd /home/worker/exo && exec {bin} --exoharness-url {url} {bearer}--harness exoclaw \
         conversation send {agent} {conversation} \"$EXO_POLICY_REPL_LINE\"",
        bin = POLICY_CONTAINER_EXO_BIN,
        url = eh_url,
        bearer = bearer_flag,
        agent = agent_slug,
        conversation = conversation_slug,
    );

    let process = agent
        .run_in_sandbox(RunInSandboxRequest {
            id: handle.sandbox_id,
            command: vec!["bash".to_string(), "-lc".to_string(), script],
            env,
        })
        .await?;
    read_process_stdout(process).await
}

// Drain a sandbox process and return its stdout, erroring (with stderr) on a
// non-zero exit. Mirrors harness_tool::read_shell_process but yields the raw
// reply text rather than a ShellToolResult.
async fn read_process_stdout(process: Box<dyn SandboxProcess>) -> Result<String> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = futures::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );
    stdout_result?;
    stderr_result?;
    let exit_code = wait_result?;

    let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    if exit_code != 0 {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        anyhow::bail!(
            "policy turn failed (exit {exit_code}):\n{}",
            stderr.trim_end()
        );
    }
    Ok(stdout)
}

pub(crate) async fn ensure_policy_sandbox(
    agent: &dyn AgentHandle,
    agent_config: &AgentConfig,
    conversation_config: &ConversationConfig,
) -> Result<PolicySandboxHandle> {
    let spec = policy_sandbox_spec(agent_config, conversation_config);
    if let Some(handle) = current_policy_sandbox(agent, &spec).await? {
        return Ok(handle);
    }

    let sandbox_name = new_policy_sandbox_name();
    let sandbox_id = agent
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

    Ok(PolicySandboxHandle { sandbox_id })
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
    let sandbox_id = agent
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
    Ok(Some(PolicySandboxHandle { sandbox_id }))
}

async fn load_policy_sandbox_record(
    agent: &dyn AgentHandle,
) -> Result<Option<PolicySandboxRecord>> {
    let Some(artifact) = latest_policy_artifact(agent, POLICY_SANDBOX_ARTIFACT_PATH).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&artifact.contents)?))
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

// Reads the newest version of an agent artifact by path (mirrors agent_sandbox).
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

fn new_policy_sandbox_name() -> String {
    format!("{POLICY_SANDBOX_NAME_PREFIX}-{}", Uuid7::now())
}
