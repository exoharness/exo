use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use exoharness::Uuid7;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterSource {
    BuiltIn,
    Library,
    Agent,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterKind {
    Worker,
    Module,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterBuildStatus {
    #[default]
    NotRequired,
    Pending,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdapterConfig {
    Worker(WorkerAdapterConfig),
    Module(ModuleAdapterConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerAdapterConfig {
    pub adapter_type: String,
    pub worker_command: Vec<String>,
    #[serde(default)]
    pub initialization: Value,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub state_dir: Option<String>,
    #[serde(default)]
    pub secret_env: Vec<WorkerSecretEnvVar>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkerSecretEnvVar {
    pub env: String,
    pub secret_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModuleAdapterConfig {
    pub module_path: String,
    #[serde(default)]
    pub initialization: Value,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterRecord {
    pub id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub source: AdapterSource,
    pub kind: AdapterKind,
    pub enabled: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub config: AdapterConfig,
    pub build_status: AdapterBuildStatus,
    pub build_error: Option<String>,
    pub latest_event_artifact_id: Option<String>,
    pub last_connected_at_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NewAdapter {
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub source: AdapterSource,
    pub config: AdapterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterEventRecord {
    pub id: String,
    pub adapter_id: String,
    pub event_type: AdapterEventType,
    pub created_at_ms: u64,
    pub artifact_id: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdapterOutboundMessageRecord {
    pub id: String,
    pub adapter_id: String,
    pub created_at_ms: u64,
    pub text: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterEventType {
    Connected,
    Inbound,
    Outbound,
    Error,
}

impl AdapterRecord {
    pub fn new(request: NewAdapter, now_ms: u64) -> Result<Self> {
        validate_adapter_name(&request.name)?;
        request.config.validate()?;
        let kind = request.config.kind();
        let build_status = match request.source {
            AdapterSource::BuiltIn => AdapterBuildStatus::NotRequired,
            AdapterSource::Library | AdapterSource::Agent => AdapterBuildStatus::Pending,
        };
        Ok(Self {
            id: Uuid7::now().to_string(),
            agent_id: non_empty("agentId", request.agent_id)?,
            conversation_id: non_empty("conversationId", request.conversation_id)?,
            name: request.name,
            source: request.source,
            kind,
            enabled: true,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            config: request.config,
            build_status,
            build_error: None,
            latest_event_artifact_id: None,
            last_connected_at_ms: None,
            last_error: None,
        })
    }
}

impl AdapterConfig {
    pub fn kind(&self) -> AdapterKind {
        match self {
            Self::Worker(_) => AdapterKind::Worker,
            Self::Module(_) => AdapterKind::Module,
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Worker(config) => config.validate(),
            Self::Module(config) => config.validate(),
        }
    }
}

impl WorkerAdapterConfig {
    fn validate(&self) -> Result<()> {
        non_empty_ref("adapterType", &self.adapter_type)?;
        if self.worker_command.is_empty() {
            bail!("worker adapter workerCommand must not be empty");
        }
        for arg in &self.worker_command {
            non_empty_ref("workerCommand item", arg)?;
        }
        if let Some(state_dir) = &self.state_dir {
            non_empty_ref("stateDir", state_dir)?;
        }
        for secret in &self.secret_env {
            non_empty_ref("secretEnv env", &secret.env)?;
            non_empty_ref("secretEnv secretId", &secret.secret_id)?;
        }
        Ok(())
    }
}

impl ModuleAdapterConfig {
    fn validate(&self) -> Result<()> {
        non_empty_ref("modulePath", &self.module_path)?;
        Ok(())
    }
}

impl AdapterEventRecord {
    pub fn new(
        adapter_id: String,
        event_type: AdapterEventType,
        summary: String,
        artifact_id: Option<String>,
        now_ms: u64,
    ) -> Result<Self> {
        Ok(Self {
            id: Uuid7::now().to_string(),
            adapter_id: non_empty("adapterId", adapter_id)?,
            event_type,
            created_at_ms: now_ms,
            artifact_id,
            summary,
        })
    }
}

impl AdapterOutboundMessageRecord {
    pub fn new(
        adapter_id: String,
        text: String,
        target: Option<String>,
        now_ms: u64,
    ) -> Result<Self> {
        if let Some(target) = &target {
            non_empty_ref("target", target)?;
        }
        Ok(Self {
            id: Uuid7::now().to_string(),
            adapter_id: non_empty("adapterId", adapter_id)?,
            created_at_ms: now_ms,
            text: non_empty("text", text)?,
            target,
        })
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_millis() as u64
}

fn validate_adapter_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("adapter name must not be empty");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        bail!("adapter name may only contain letters, numbers, '-' and '_'");
    }
    Ok(())
}

fn non_empty(field: &str, value: String) -> Result<String> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(value)
}

fn non_empty_ref(field: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{field} must not be empty");
    }
    Ok(())
}
