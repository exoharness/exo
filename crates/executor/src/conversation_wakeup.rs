use exoharness::Result;
use lingua::Message;
use lingua::universal::UserContent;

use crate::{HarnessConversation, SendRequest, SendResult};

/// Wake a conversation with an external prompt (scheduler results, adapter
/// messages). Serialization against other senders — including senders in
/// other processes — comes from the substrate's turn coordinator inside
/// `send()`, so a wakeup is just a normal send in a fresh session.
pub async fn send_conversation_wakeup(
    conversation: &dyn HarnessConversation,
    prompt: String,
) -> Result<SendResult> {
    send_conversation_wakeup_content(conversation, UserContent::String(prompt)).await
}

/// Wakeup variant for multimodal content, e.g. adapter messages that carry
/// inbound images for the model to analyze.
pub async fn send_conversation_wakeup_content(
    conversation: &dyn HarnessConversation,
    content: UserContent,
) -> Result<SendResult> {
    let result = conversation
        .send(SendRequest {
            input: vec![Message::User { content }],
            session_id: None,
        })
        .await?;
    conversation.close_session(result.session_id).await?;
    Ok(result)
}
