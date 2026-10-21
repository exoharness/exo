use crate::ExecutionStreamEvent;
use anyhow::Result;
use exoharness::ToolArguments;
use lingua::{
    Message,
    universal::{AssistantContent, AssistantContentPart},
};
use serde::{Deserialize, Serialize};
use slack_morphism::prelude::{SlackStreamChunk, SlackTaskCardStatus};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Progress {
    tools: BTreeMap<String, String>,
    running: BTreeSet<String>,
    text_open: bool,
    has_text: bool,
    last_text: String,
}

impl Progress {
    pub fn event(&mut self, event: ExecutionStreamEvent) -> Result<Vec<SlackStreamChunk>> {
        Ok(match event {
            ExecutionStreamEvent::Messages { messages } => {
                let mut chunks = Vec::new();
                for message in messages {
                    if let Message::Assistant { content, .. } = message {
                        let text = match content {
                            AssistantContent::String(text) => text,
                            AssistantContent::Array(parts) => parts
                                .into_iter()
                                .filter_map(|part| {
                                    if let AssistantContentPart::Text(text) = part {
                                        Some(text.text)
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                        };
                        if !text.is_empty() {
                            chunks.extend(self.message(&text));
                            self.text_open = false;
                        }
                    }
                }
                chunks
            }
            ExecutionStreamEvent::Checkpoint { .. } => Vec::new(),
            ExecutionStreamEvent::Status { message } => {
                vec![Self::status(&message)]
            }
            ExecutionStreamEvent::Chunk(chunk) => {
                let mut text = String::new();
                for choice in &chunk.choices {
                    if let Some(delta) = choice.delta_view()
                        && let Some(content) = delta.content
                    {
                        text.push_str(&content);
                    }
                }
                self.text(&text)
            }
            ExecutionStreamEvent::ToolCall {
                tool_call_id,
                tool_name,
                arguments,
            } => self.tool_call(&tool_call_id, &tool_name, &arguments)?,
            ExecutionStreamEvent::ToolResult { tool_call_id, .. } => {
                self.tool_result(&tool_call_id)
            }
            ExecutionStreamEvent::ApprovalRequested { .. } => {
                vec![Self::status("Waiting for your approval")]
            }
            ExecutionStreamEvent::Suspended(_) => {
                vec![Self::status("Paused")]
            }
            ExecutionStreamEvent::FirstChunk { .. } => {
                vec![Self::status("Responding")]
            }
            ExecutionStreamEvent::Completed(_) => Vec::new(),
        })
    }

    pub fn status(title: &str) -> SlackStreamChunk {
        task("exo-status", title, SlackTaskCardStatus::InProgress, None)
    }

    pub fn last_message(&self) -> Option<&str> {
        (!self.last_text.is_empty()).then_some(&self.last_text)
    }

    fn text(&mut self, text: &str) -> Vec<SlackStreamChunk> {
        if text.is_empty() {
            return Vec::new();
        }
        let mut result = Vec::new();
        if !self.text_open {
            if self.has_text {
                result.push(SlackStreamChunk::MarkdownText {
                    text: "\n\n".into(),
                });
            }
            self.last_text.clear();
        }
        self.text_open = true;
        self.has_text = true;
        self.last_text.push_str(text);
        // Slack limits each markdown append to 12,000 characters.
        result.extend(text.chars().collect::<Vec<_>>().chunks(4000).map(|part| {
            SlackStreamChunk::MarkdownText {
                text: part.iter().collect(),
            }
        }));
        result
    }

    pub fn message(&mut self, text: &str) -> Vec<SlackStreamChunk> {
        // A recovered stream may already contain a prefix of this committed message.
        if text == self.last_text {
            return Vec::new();
        }
        if self.text_open
            && let Some(tail) = text.strip_prefix(&self.last_text)
        {
            self.text(tail)
        } else {
            self.text_open = false;
            self.text(text)
        }
    }

    pub fn tool_call(
        &mut self,
        id: &str,
        name: &str,
        arguments: &ToolArguments,
    ) -> Result<Vec<SlackStreamChunk>> {
        self.text_open = false;
        self.tools.insert(id.into(), name.into());
        self.running.insert(id.into());
        Ok(vec![
            task(
                id,
                name,
                SlackTaskCardStatus::InProgress,
                Some(serde_json::to_string(arguments)?),
            ),
            Self::status("Running tools"),
        ])
    }

    pub fn tool_result(&mut self, id: &str) -> Vec<SlackStreamChunk> {
        self.running.remove(id);
        let mut result = Vec::new();
        if let Some(name) = self.tools.get(id) {
            result.push(task(id, name, SlackTaskCardStatus::Complete, None));
        }
        result.push(Self::status(if self.running.is_empty() {
            "Waiting for model"
        } else {
            "Running tools"
        }));
        result
    }

    pub fn finish(&mut self, text: &str, failure: Option<&str>) -> Vec<SlackStreamChunk> {
        let mut chunks = self.message(text);
        chunks.push(task(
            "exo-status",
            failure.unwrap_or("Finished"),
            if failure.is_some() {
                SlackTaskCardStatus::Error
            } else {
                SlackTaskCardStatus::Complete
            },
            None,
        ));
        chunks
    }
}

pub(super) fn batch(chunks: &[SlackStreamChunk]) -> &[SlackStreamChunk] {
    let mut size = 0;
    let mut count = 0;
    for chunk in chunks {
        let length = match chunk {
            SlackStreamChunk::MarkdownText { text } => text.chars().count(),
            _ => 512,
        };
        if size + length > 10_000 {
            break;
        }
        size += length;
        count += 1;
    }
    &chunks[..count]
}

fn task(
    id: &str,
    title: &str,
    status: SlackTaskCardStatus,
    details: Option<String>,
) -> SlackStreamChunk {
    SlackStreamChunk::TaskUpdate {
        id: id.into(),
        title: title.chars().take(256).collect(),
        status,
        details: details.map(|value| value.chars().take(256).collect()),
        hide_title: None,
        icon: None,
        output: None,
        sources: None,
    }
}
