use exoharness::{AgentHandle, ConversationHandle, Result, ToolRequest, ToolResult};
use serde::Deserialize;
use serde_json::Value;

use super::runtime::send_adapter_message_with_handles;
use super::store::AdapterStore;
use super::types::{AdapterConfig, AdapterSource, NewAdapter};

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
    send_adapter_message_with_handles(
        agent,
        conversation,
        store,
        &adapter,
        &args.text,
        args.target.as_deref(),
    )
    .await?;
    Ok(serde_json::json!({
        "ok": true,
        "adapterId": args.adapter_id,
        "sent": true,
    }))
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
                        "type": "worker",
                        "adapterType": "irc",
                        "workerCommand": ["node", "irc.js"],
                        "initialization": {},
                        "capabilities": ["receive", "send_message"],
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
