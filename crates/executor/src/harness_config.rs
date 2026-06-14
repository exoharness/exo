use anyhow::anyhow;
use exoharness::{
    AgentHandle, ConversationHandle, ReadArtifactRequest, Result, WriteArtifactRequest,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::{AgentConfig, ConversationConfig};

pub(crate) const AGENT_CONFIG_ARTIFACT_PATH: &str = "config/executor.json";
pub(crate) const CONVERSATION_CONFIG_ARTIFACT_PATH: &str = "config/executor.json";

pub async fn load_agent_config(agent: &dyn AgentHandle) -> Result<AgentConfig> {
    read_json_artifact_from_agent(agent, AGENT_CONFIG_ARTIFACT_PATH)
        .await?
        .ok_or_else(|| anyhow!("missing agent config artifact at {AGENT_CONFIG_ARTIFACT_PATH}"))
}

pub async fn store_agent_config(agent: &dyn AgentHandle, config: &AgentConfig) -> Result<()> {
    write_json_artifact_to_agent(agent, AGENT_CONFIG_ARTIFACT_PATH, config).await
}

pub async fn load_conversation_config(
    conversation: &dyn ConversationHandle,
) -> Result<ConversationConfig> {
    Ok(
        read_json_artifact_from_conversation(conversation, CONVERSATION_CONFIG_ARTIFACT_PATH)
            .await?
            .unwrap_or_default(),
    )
}

pub async fn store_conversation_config(
    conversation: &dyn ConversationHandle,
    config: &ConversationConfig,
) -> Result<()> {
    write_json_artifact_to_conversation(conversation, CONVERSATION_CONFIG_ARTIFACT_PATH, config)
        .await
}

async fn write_json_artifact_to_agent<T: Serialize>(
    agent: &dyn AgentHandle,
    path: &str,
    value: &T,
) -> Result<()> {
    agent
        .write_artifact(WriteArtifactRequest {
            path: path.to_string(),
            contents: serde_json::to_vec_pretty(value)?,
        })
        .await?;
    Ok(())
}

async fn write_json_artifact_to_conversation<T: Serialize>(
    conversation: &dyn ConversationHandle,
    path: &str,
    value: &T,
) -> Result<()> {
    conversation
        .write_artifact(WriteArtifactRequest {
            path: path.to_string(),
            contents: serde_json::to_vec_pretty(value)?,
        })
        .await?;
    Ok(())
}

async fn read_json_artifact_from_agent<T: DeserializeOwned>(
    agent: &dyn AgentHandle,
    path: &str,
) -> Result<Option<T>> {
    let Some(artifact) = agent.read_artifact(ReadArtifactRequest::path(path)).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&artifact.contents)?))
}

async fn read_json_artifact_from_conversation<T: DeserializeOwned>(
    conversation: &dyn ConversationHandle,
    path: &str,
) -> Result<Option<T>> {
    let Some(artifact) = conversation
        .read_artifact(ReadArtifactRequest::path(path))
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&artifact.contents)?))
}
