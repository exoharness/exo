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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AdapterConfig {
    pub adapter_type: String,
    pub worker_command: Vec<String>,
    #[serde(default)]
    pub initialization: Value,
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
pub struct AdapterRecord {
    pub id: String,
    pub agent_id: String,
    pub conversation_id: String,
    pub name: String,
    pub source: AdapterSource,
    pub enabled: bool,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub config: AdapterConfig,
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
        Ok(Self {
            id: Uuid7::now().to_string(),
            agent_id: non_empty("agentId", request.agent_id)?,
            conversation_id: non_empty("conversationId", request.conversation_id)?,
            name: request.name,
            source: request.source,
            enabled: true,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            config: request.config,
            last_connected_at_ms: None,
            last_error: None,
        })
    }
}

impl AdapterConfig {
    pub fn validate(&self) -> Result<()> {
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

impl AdapterEventRecord {
    pub fn new(
        adapter_id: String,
        event_type: AdapterEventType,
        summary: String,
        now_ms: u64,
    ) -> Result<Self> {
        Ok(Self {
            id: Uuid7::now().to_string(),
            adapter_id: non_empty("adapterId", adapter_id)?,
            event_type,
            created_at_ms: now_ms,
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
