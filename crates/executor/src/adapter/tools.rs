use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine;
use exoharness::{
    AgentHandle, ConversationHandle, Result, RunInSandboxRequest, SandboxProcess, ToolRequest,
    ToolResult, Uuid7,
};
use futures::io::AsyncReadExt;
use serde::Deserialize;
use serde_json::Value;
use tokio::fs;

use super::runtime::send_adapter_message_with_handles;
use super::store::AdapterStore;
use super::types::{AdapterAttachment, AdapterConfig, AdapterSource, NewAdapter};
use crate::agent_sandbox::ensure_agent_sandbox;
use crate::conversation_sandbox::ensure_conversation_sandbox;
use crate::{AgentConfig, ConversationConfig, SandboxScope, effective_sandbox_scope};

const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAdapterArguments {
    agent_id: String,
    conversation_id: String,
    name: String,
    source: AdapterSource,
    config: AdapterConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConversationScopedArguments {
    agent_id: String,
    conversation_id: String,
    include_disabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdapterIdArguments {
    agent_id: String,
    conversation_id: String,
    adapter_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendAdapterMessageArguments {
    agent_id: String,
    conversation_id: String,
    adapter_id: String,
    text: String,
    target: Option<String>,
    #[serde(default)]
    attachments: Option<Vec<AdapterAttachment>>,
}

pub async fn execute_create_adapter_tool(
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<CreateAdapterArguments>(Value::Object(request.arguments.clone()))?;
    let adapter = store
        .create_adapter(NewAdapter {
            agent_id: args.agent_id,
            conversation_id: args.conversation_id,
            name: args.name,
            source: args.source,
            config: args.config,
        })
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapter": adapter,
    }))
}

pub async fn execute_list_adapters_tool(
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<ConversationScopedArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let adapters = store
        .list_adapters_for_conversation(
            &args.agent_id,
            &args.conversation_id,
            args.include_disabled.unwrap_or(false),
        )
        .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapters": adapters,
    }))
}

pub async fn execute_disable_adapter_tool(
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<AdapterIdArguments>(Value::Object(request.arguments.clone()))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != args.agent_id || adapter.conversation_id != args.conversation_id {
        return Ok(not_found());
    }
    let _ = conversation.record();
    store.disable_adapter(&args.adapter_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "disabled": true,
    }))
}

pub async fn execute_delete_adapter_tool(
    conversation: &dyn ConversationHandle,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args =
        serde_json::from_value::<AdapterIdArguments>(Value::Object(request.arguments.clone()))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != args.agent_id || adapter.conversation_id != args.conversation_id {
        return Ok(not_found());
    }
    let _ = conversation.record();
    store.delete_adapter(&args.adapter_id).await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "deleted": true,
        "eventsDeleted": true,
    }))
}

pub async fn execute_send_adapter_message_tool(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    store: &AdapterStore,
    request: &ToolRequest,
) -> Result<ToolResult> {
    let args = serde_json::from_value::<SendAdapterMessageArguments>(Value::Object(
        request.arguments.clone(),
    ))?;
    let Some(adapter) = store.get_adapter(&args.adapter_id).await? else {
        return Ok(not_found());
    };
    if adapter.agent_id != args.agent_id || adapter.conversation_id != args.conversation_id {
        return Ok(not_found());
    }
    let attachments = args.attachments.unwrap_or_default();
    if !attachments.is_empty()
        && adapter.config.adapter_type != "whatsapp"
        && adapter.config.adapter_type != "signal"
        && adapter.config.adapter_type != "discord"
    {
        bail!(
            "adapter {} does not support rich attachments",
            adapter.config.adapter_type
        );
    }
    let attachments = resolve_sandbox_attachments(
        agent,
        conversation,
        agent_config,
        config,
        store,
        &adapter,
        attachments,
    )
    .await?;
    send_adapter_message_with_handles(
        agent,
        conversation,
        store,
        &adapter,
        &args.text,
        args.target.as_deref(),
        attachments,
    )
    .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "sent": true,
    }))
}

async fn resolve_sandbox_attachments(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    store: &AdapterStore,
    adapter: &super::types::AdapterRecord,
    attachments: Vec<AdapterAttachment>,
) -> Result<Vec<AdapterAttachment>> {
    let mut resolved = Vec::with_capacity(attachments.len());
    for mut attachment in attachments {
        attachment.validate()?;
        if adapter.config.adapter_type == "signal"
            && let Some(url) = attachment.url.clone()
        {
            let bytes = download_attachment(&url).await?;
            let path = stage_attachment(store, &adapter.id, &url, &attachment, bytes).await?;
            attachment.path = Some(path.to_string_lossy().into_owned());
            attachment.url = None;
            resolved.push(attachment);
            continue;
        }
        let Some(sandbox_path) = attachment.sandbox_path.clone() else {
            resolved.push(attachment);
            continue;
        };
        if attachment.path.is_some() || attachment.url.is_some() || attachment.data.is_some() {
            bail!("attachment sandboxPath cannot be combined with path, url, or data");
        }
        let bytes =
            read_sandbox_file(agent, conversation, agent_config, config, &sandbox_path).await?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            bail!(
                "sandbox attachment is too large: {} bytes exceeds {} bytes",
                bytes.len(),
                MAX_ATTACHMENT_BYTES
            );
        }
        let path = stage_attachment(store, &adapter.id, &sandbox_path, &attachment, bytes).await?;
        attachment.path = Some(path.to_string_lossy().into_owned());
        attachment.sandbox_path = None;
        resolved.push(attachment);
    }
    Ok(resolved)
}

async fn read_sandbox_file(
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    agent_config: &AgentConfig,
    config: &ConversationConfig,
    sandbox_path: &str,
) -> Result<Vec<u8>> {
    let encoded = match effective_sandbox_scope(agent_config, config) {
        SandboxScope::Agent => {
            let sandbox = ensure_agent_sandbox(agent, conversation, agent_config, config).await?;
            read_sandbox_file_base64(
                sandbox.conversation.as_ref(),
                sandbox.sandbox_id,
                sandbox_path,
            )
            .await?
        }
        SandboxScope::Conversation => {
            let sandbox_id =
                ensure_conversation_sandbox(conversation, agent_config, config).await?;
            read_sandbox_file_base64(conversation, sandbox_id, sandbox_path).await?
        }
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("failed to decode sandbox attachment")?;
    Ok(bytes)
}

async fn download_attachment(url: &str) -> Result<Vec<u8>> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("failed to download adapter attachment {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download adapter attachment {url}"))?;
    if let Some(content_length) = response.content_length()
        && content_length > MAX_ATTACHMENT_BYTES as u64
    {
        bail!(
            "attachment is too large: {} bytes exceeds {} bytes",
            content_length,
            MAX_ATTACHMENT_BYTES
        );
    }
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read adapter attachment {url}"))?;
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        bail!(
            "attachment is too large: {} bytes exceeds {} bytes",
            bytes.len(),
            MAX_ATTACHMENT_BYTES
        );
    }
    Ok(bytes.to_vec())
}

async fn read_sandbox_file_base64(
    conversation: &dyn ConversationHandle,
    sandbox_id: String,
    sandbox_path: &str,
) -> Result<String> {
    let process = conversation
        .run_in_sandbox(RunInSandboxRequest {
            id: sandbox_id,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "base64 \"$1\" | tr -d '\\n'".to_string(),
                "exo-read-attachment".to_string(),
                sandbox_path.to_string(),
            ],
            env: Default::default(),
        })
        .await?;
    let output = read_process(process).await?;
    if output.exit_code != 0 {
        bail!(
            "failed to read sandbox attachment {}: {}",
            sandbox_path,
            output.stderr.trim()
        );
    }
    Ok(output.stdout)
}

struct ProcessOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

async fn read_process(process: Box<dyn SandboxProcess>) -> Result<ProcessOutput> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = tokio::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );
    stdout_result?;
    stderr_result?;
    let exit_code = wait_result?;

    Ok(ProcessOutput {
        stdout: String::from_utf8(stdout_bytes).context("sandbox attachment was not utf-8")?,
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
    })
}

async fn stage_attachment(
    store: &AdapterStore,
    adapter_id: &str,
    sandbox_path: &str,
    attachment: &AdapterAttachment,
    bytes: Vec<u8>,
) -> Result<PathBuf> {
    let media_dir = store.root().join("media").join(adapter_id);
    fs::create_dir_all(&media_dir).await?;
    let file_name = staged_file_name(sandbox_path, attachment);
    let path = media_dir.join(format!("{}-{file_name}", Uuid7::now()));
    fs::write(&path, bytes)
        .await
        .with_context(|| format!("failed to stage adapter attachment {}", path.display()))?;
    Ok(path)
}

fn staged_file_name(sandbox_path: &str, attachment: &AdapterAttachment) -> String {
    let raw = attachment
        .file_name
        .as_deref()
        .or_else(|| {
            Path::new(sandbox_path)
                .file_name()
                .and_then(|name| name.to_str())
        })
        .unwrap_or("attachment");
    let sanitized = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized
    }
}

fn not_found() -> ToolResult {
    serde_json::json!({
        "ok": false,
        "error": "adapter not found for this conversation",
    })
}

#[cfg(test)]
mod tests {
    use exoharness::ToolRequest;
    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn create_and_list_adapter_tools_are_conversation_scoped() {
        let tempdir = TempDir::new().unwrap();
        let store = AdapterStore::new(tempdir.path());
        let create_result = execute_create_adapter_tool(
            &store,
            &ToolRequest {
                function_name: "create_adapter".to_string(),
                arguments: serde_json::json!({
                    "agentId": "agent",
                    "conversationId": "conversation",
                    "name": "irc",
                    "source": "built_in",
                    "config": {
                        "adapterType": "irc",
                        "workerCommand": ["node", "irc.js"],
                        "initialization": {},
                        "stateDir": null,
                        "secretEnv": []
                    }
                })
                .as_object()
                .unwrap()
                .clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(create_result["ok"], true);

        let list_result = execute_list_adapters_tool(
            &store,
            &ToolRequest {
                function_name: "list_adapters".to_string(),
                arguments: serde_json::json!({
                    "agentId": "agent",
                    "conversationId": "conversation",
                    "includeDisabled": false
                })
                .as_object()
                .unwrap()
                .clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(list_result["adapters"].as_array().unwrap().len(), 1);
    }
}
