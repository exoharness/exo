use std::collections::HashMap;

use exoharness::{EventData, ToolCallId};
use lingua::Message;
use lingua::universal::{
    AssistantContent, AssistantContentPart, ToolCallArguments, ToolContentPart,
    ToolResultContentPart,
};
use serde_json::json;

use crate::harness_helpers::to_lingua_value;

/// Each batch must contain complete tool rounds: calls still pending at its end
/// receive cancellation results, and their later real results would be discarded.
pub(crate) fn extend_message_history(
    history: &mut Vec<Message>,
    tool_call_names: &mut HashMap<ToolCallId, String>,
    events: &[exoharness::Event],
) -> anyhow::Result<()> {
    let mut pending_tool_call_ids = Vec::new();
    let mut model_inputs = HashMap::new();
    let mut client_messages = Vec::new();
    for event in events {
        if let Some((id, content)) = crate::frontend_tools::model_input(event)? {
            model_inputs.insert(id, content);
        }
    }

    for event in events {
        match &event.data {
            EventData::Messages { messages, .. } => {
                for message in messages {
                    match message {
                        Message::Tool { content } => {
                            for ToolContentPart::ToolResult(result) in content {
                                remove_pending_tool_call(
                                    &mut pending_tool_call_ids,
                                    &result.tool_call_id,
                                );
                            }
                        }
                        _ => {
                            flush_dangling_tool_results(
                                history,
                                tool_call_names,
                                &mut pending_tool_call_ids,
                            );
                            history.append(&mut client_messages);
                        }
                    }
                    if let Message::Assistant {
                        content: AssistantContent::Array(parts),
                        ..
                    } = message
                    {
                        for part in parts {
                            if let AssistantContentPart::ToolCall {
                                tool_call_id,
                                tool_name,
                                ..
                            } = part
                            {
                                tool_call_names.insert(tool_call_id.clone(), tool_name.clone());
                                pending_tool_call_ids.push(tool_call_id.clone());
                            }
                        }
                    }
                    history.push(message.clone());
                }
            }
            EventData::ToolRequested {
                tool_call_id,
                request,
                ..
            } if !tool_call_names.contains_key(tool_call_id) => {
                history.push(Message::Assistant {
                    content: AssistantContent::Array(vec![AssistantContentPart::ToolCall {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: request.function_name.clone(),
                        arguments: ToolCallArguments::Valid(
                            request
                                .arguments
                                .iter()
                                .map(|(key, value)| (key.clone(), to_lingua_value(value.clone())))
                                .collect(),
                        ),
                        encrypted_content: None,
                        provider_options: None,
                        provider_executed: None,
                    }]),
                    id: None,
                });
                tool_call_names.insert(tool_call_id.clone(), request.function_name.clone());
                pending_tool_call_ids.push(tool_call_id.clone());
            }
            EventData::ToolResult {
                tool_call_id,
                result,
            } => {
                if !pending_tool_call_ids.contains(tool_call_id) {
                    continue;
                }
                let Some(tool_name) = tool_call_names.get(tool_call_id) else {
                    continue;
                };
                remove_pending_tool_call(&mut pending_tool_call_ids, tool_call_id);
                history.push(Message::Tool {
                    content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        output: to_lingua_value(result.clone()),
                        provider_options: None,
                    })],
                });
                if let Some(content) = model_inputs.remove(tool_call_id) {
                    client_messages.push(Message::User { content });
                }
                if pending_tool_call_ids.is_empty() {
                    history.append(&mut client_messages);
                }
            }
            _ => {}
        }
    }
    flush_dangling_tool_results(history, tool_call_names, &mut pending_tool_call_ids);
    history.append(&mut client_messages);
    Ok(())
}

fn flush_dangling_tool_results(
    history: &mut Vec<Message>,
    tool_call_names: &HashMap<ToolCallId, String>,
    pending_tool_call_ids: &mut Vec<ToolCallId>,
) {
    for tool_call_id in std::mem::take(pending_tool_call_ids) {
        let Some(tool_name) = tool_call_names.get(&tool_call_id) else {
            continue;
        };
        history.push(Message::Tool {
            content: vec![ToolContentPart::ToolResult(ToolResultContentPart {
                tool_call_id,
                tool_name: tool_name.clone(),
                output: to_lingua_value(json!({
                    "ok": false,
                    "error": "tool execution did not complete before the previous turn ended",
                })),
                provider_options: None,
            })],
        });
    }
}

fn remove_pending_tool_call(pending_tool_call_ids: &mut Vec<ToolCallId>, tool_call_id: &str) {
    if let Some(index) = pending_tool_call_ids
        .iter()
        .position(|pending| pending == tool_call_id)
    {
        pending_tool_call_ids.remove(index);
    }
}
