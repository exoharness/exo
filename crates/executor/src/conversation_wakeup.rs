use exoharness::Result;
use lingua::Message;
use lingua::universal::UserContent;

use crate::{HarnessConversation, SendRequest, SendResult};

pub async fn send_conversation_wakeup(
    conversation: &dyn HarnessConversation,
    prompt: String,
) -> Result<SendResult> {
    let result = conversation
        .send(SendRequest {
            input: vec![Message::User {
                content: UserContent::String(prompt),
            }],
            session_id: None,
        })
        .await?;
    conversation.close_session(result.session_id).await?;
    Ok(result)
}
