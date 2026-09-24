use std::{collections::BTreeMap, ops::Bound};

use anyhow::{Context, Result, bail, ensure};
use exo_managed_agents::{http::protocol::ApprovalResponseBody, permissions::PermissionPolicy};
use exoharness::{
    Event, EventData, EventId, EventKind, EventQuery, EventQueryDirection, ThreadHandle,
    ToolRequest, TurnHandle, TurnRecord, Uuid7,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{ExecutionStreamEvent, harness_executor::ExecutorStreamMode};

pub(crate) const APPROVAL_REQUESTED: &str = "agent_runtime.approval_requested";
const APPROVAL_RESPONSE: &str = "agent_runtime.approval_response";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub request: ToolRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ApprovalResponse {
    approval_id: String,
    approved: bool,
    allowed_tool_name: Option<String>,
}

pub(crate) async fn approval_events(
    thread: &dyn ThreadHandle,
    mut query: EventQuery,
) -> Result<Vec<Event>> {
    query.direction = Some(EventQueryDirection::Asc);
    query.types = Some(vec![
        EventKind::custom(APPROVAL_REQUESTED),
        EventKind::custom(APPROVAL_RESPONSE),
        EventKind::TURN_ENDED,
    ]);
    query.limit = Some(100);
    let mut events = Vec::new();
    loop {
        let page = thread.get_events(Some(query.clone())).await?;
        events.extend(page.events);
        match page.cursor {
            Some(cursor) => query.cursor = Some(cursor),
            None => return Ok(events),
        }
    }
}

async fn pending(thread: &dyn ThreadHandle, turn: &TurnRecord) -> Result<Vec<ApprovalRequest>> {
    let events = approval_events(
        thread,
        EventQuery {
            turn_id: Some(turn.id),
            session_id: Some(turn.session_id),
            ..Default::default()
        },
    )
    .await?;
    pending_from_events(events)
}

pub(crate) fn pending_from_events(events: Vec<Event>) -> Result<Vec<ApprovalRequest>> {
    let mut pending = BTreeMap::new();
    for event in events {
        match event.data {
            EventData::Custom {
                event_type,
                payload,
            } if event_type == APPROVAL_REQUESTED => {
                let request: ApprovalRequest = serde_json::from_value(payload)?;
                pending.insert(request.approval_id.clone(), request);
            }
            EventData::Custom {
                event_type,
                payload,
            } if event_type == APPROVAL_RESPONSE => {
                let response: ApprovalResponse = serde_json::from_value(payload)?;
                pending.remove(&response.approval_id);
            }
            EventData::TurnEnded => return Ok(Vec::new()),
            _ => {}
        }
    }
    Ok(pending.into_values().collect())
}

pub(crate) async fn respond(
    thread: &dyn ThreadHandle,
    turn: TurnRecord,
    body: &ApprovalResponseBody,
) -> Result<EventId> {
    ensure!(
        !body.allow_for_tool || body.approved,
        "a denied request cannot allow future calls"
    );
    let request = pending(thread, &turn)
        .await?
        .into_iter()
        .find(|request| request.approval_id == body.approval_id)
        .context("approval is not pending for this session and turn")?;
    let payload = ApprovalResponse {
        approval_id: body.approval_id.clone(),
        approved: body.approved,
        allowed_tool_name: body.allow_for_tool.then_some(request.request.function_name),
    };
    Ok(thread
        .turn_handle(turn)
        .await?
        .add_events(vec![EventData::Custom {
            event_type: APPROVAL_RESPONSE.to_owned(),
            payload: serde_json::to_value(payload)?,
        }])
        .await?
        .latest_event_id)
}

pub(crate) async fn authorize(
    thread: &dyn ThreadHandle,
    turn: &dyn TurnHandle,
    policy: PermissionPolicy,
    request: &ToolRequest,
    stream: ExecutorStreamMode<'_>,
) -> Result<()> {
    if matches!(policy, PermissionPolicy::AlwaysAllow {}) {
        return Ok(());
    }
    for event in approval_events(
        thread,
        EventQuery {
            session_id: Some(turn.record().session_id),
            ..Default::default()
        },
    )
    .await?
    {
        if let EventData::Custom {
            event_type,
            payload,
        } = event.data
            && event_type == APPROVAL_RESPONSE
        {
            let response: ApprovalResponse = serde_json::from_value(payload)?;
            if response.approved
                && response.allowed_tool_name.as_deref() == Some(&request.function_name)
            {
                return Ok(());
            }
        }
    }
    let approval = ApprovalRequest {
        approval_id: Uuid7::now().to_string(),
        request: request.clone(),
    };
    let appended = turn
        .add_events(vec![EventData::Custom {
            event_type: APPROVAL_REQUESTED.to_owned(),
            payload: serde_json::to_value(&approval)?,
        }])
        .await?;
    let mut events = thread
        .watch_events(Bound::Excluded(appended.latest_event_id))
        .await?;
    if let ExecutorStreamMode::Enabled(sender) = stream
        && sender
            .send(Ok(ExecutionStreamEvent::ApprovalRequested {
                turn: turn.record().clone(),
                approval: approval.clone(),
            }))
            .is_err()
    {
        tracing::debug!("approval observer disconnected");
    }
    while let Some(event) = events.next().await {
        let event = event?;
        if event.turn_id != Some(turn.record().id)
            || event.session_id != Some(turn.record().session_id)
        {
            continue;
        }
        match event.data {
            EventData::Custom {
                event_type,
                payload,
            } if event_type == APPROVAL_RESPONSE => {
                let response: ApprovalResponse = serde_json::from_value(payload)?;
                if response.approval_id == approval.approval_id {
                    ensure!(
                        response.approved,
                        "tool call denied by the user: {}",
                        request.function_name
                    );
                    return Ok(());
                }
            }
            EventData::TurnEnded => bail!("turn ended while waiting for tool approval"),
            _ => {}
        }
    }
    bail!("event stream closed while waiting for tool approval")
}

pub(crate) async fn turn_caller(
    thread: &dyn exoharness::ThreadHandle,
    turn: exoharness::TurnId,
) -> anyhow::Result<Option<String>> {
    let events = thread
        .get_events(Some(exoharness::EventQuery {
            turn_id: Some(turn),
            types: Some(vec![exoharness::EventKind::TURN_STARTED]),
            limit: Some(1),
            ..Default::default()
        }))
        .await?;
    Ok(events
        .events
        .into_iter()
        .find_map(|event| match event.data {
            exoharness::EventData::TurnStarted { user_id } => user_id,
            _ => None,
        }))
}
