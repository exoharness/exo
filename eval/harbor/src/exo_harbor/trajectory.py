"""Convert an Exo trial conversation into Harbor's native ATIF trajectory.

See https://www.harborframework.com/docs/agents/trajectory-format for more details
on the AITF trajectory format."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Annotated, Literal

from harbor.models.trajectories.agent import Agent
from harbor.models.trajectories.final_metrics import FinalMetrics
from harbor.models.trajectories.metrics import Metrics
from harbor.models.trajectories.observation import Observation
from harbor.models.trajectories.observation_result import ObservationResult
from harbor.models.trajectories.step import Step
from harbor.models.trajectories.tool_call import ToolCall
from harbor.models.trajectories.trajectory import Trajectory
from pydantic import BaseModel, ConfigDict, Field, JsonValue, field_validator

from exo_harbor import conventions
from exo_harbor.exo import ExoClient


class Usage(BaseModel):
    model: str
    prompt_tokens: int
    completion_tokens: int
    prompt_cached_tokens: int = 0
    completion_reasoning_tokens: int = 0
    cost_usd: float


class TextContent(BaseModel):
    type: Literal["text"]
    text: str


class ReasoningContent(BaseModel):
    type: Literal["reasoning"]
    text: str
    encrypted_content: str | None = None


class ValidToolArguments(BaseModel):
    type: Literal["valid"]
    value: dict[str, JsonValue]


class ToolCallContent(BaseModel):
    type: Literal["tool_call"]
    tool_call_id: str
    tool_name: str
    arguments: ValidToolArguments


AssistantContent = Annotated[
    TextContent | ReasoningContent | ToolCallContent,
    Field(discriminator="type"),
]


class UserMessage(BaseModel):
    role: Literal["user"]
    content: str


class AssistantMessage(BaseModel):
    role: Literal["assistant"]
    content: list[AssistantContent]

    @field_validator("content", mode="before")
    @classmethod
    def plain_text_is_one_text_part(cls, content: object) -> object:
        # The coding-agent harnesses (pi, codex, ...) record assistant replies
        # as a bare string rather than a list of typed parts.
        if isinstance(content, str):
            return [{"type": "text", "text": content}]
        return content


Message = Annotated[UserMessage | AssistantMessage, Field(discriminator="role")]


class MessagesData(BaseModel):
    type: Literal["messages"]
    messages: list[Message]
    usage: Usage | None = None


class ToolResultValue(BaseModel):
    """A result from Exo's own tool runtime."""

    ok: bool
    preview: str
    source: str
    tool_name: str = Field(alias="toolName")
    truncated: bool
    value: JsonValue = None


class PiToolResultValue(BaseModel):
    """The pi harness records `{is_error, result}`, `result` being whatever the
    agent's tool returned, usually MCP-style `{"content": [{"type": "text", ...}]}`."""

    model_config = ConfigDict(extra="forbid")

    is_error: bool
    result: JsonValue = None

    def ok(self) -> bool:
        return not self.is_error

    def text(self) -> str:
        return content_blocks_text(self.result)


class ClaudeToolResultValue(BaseModel):
    """The claude-code harness records `{content, is_error}`, `content` being
    the tool's text or its content blocks."""

    model_config = ConfigDict(extra="forbid")

    content: JsonValue = None
    is_error: bool

    def ok(self) -> bool:
        return not self.is_error

    def text(self) -> str:
        return content_blocks_text(self.content)


class CodexCommandResultValue(BaseModel):
    """The codex harness records a shell command as
    `{status, exit_code, output, duration_ms}`."""

    model_config = ConfigDict(extra="forbid")

    status: str | None
    exit_code: int | None = None
    output: str | None = None
    duration_ms: int | None = None

    def ok(self) -> bool:
        return self.status == "completed" and self.exit_code in (None, 0)

    def text(self) -> str:
        return self.output or ""


class CodexItemResultValue(BaseModel):
    """Any other Codex item: `{status, result}`, plus `error` for MCP calls."""

    model_config = ConfigDict(extra="forbid")

    status: str | None
    result: JsonValue = None
    error: JsonValue = None

    def ok(self) -> bool:
        return self.status == "completed" and self.error is None

    def text(self) -> str:
        if self.error is not None:
            return json.dumps(self.error, indent=2)
        return content_blocks_text(self.result)


class ToolErrorValue(BaseModel):
    """A coding-agent tool call that failed before producing a result."""

    model_config = ConfigDict(extra="forbid")

    error: str

    def ok(self) -> bool:
        return False

    def text(self) -> str:
        return self.error


def content_blocks_text(value: JsonValue) -> str:
    """Text of a tool result: a bare string, MCP-style `{"content": [...]}`,
    or a list of content blocks; anything else as JSON."""
    if isinstance(value, str):
        return value
    blocks = value.get("content") if isinstance(value, dict) else value
    if isinstance(blocks, list):
        parts = [
            block["text"]
            for block in blocks
            if isinstance(block, dict) and isinstance(block.get("text"), str)
        ]
        if parts:
            return "\n".join(parts)
    return json.dumps(value, indent=2)


AgentToolResultValue = (
    PiToolResultValue
    | ClaudeToolResultValue
    | CodexCommandResultValue
    | CodexItemResultValue
    | ToolErrorValue
)


class ToolResultData(BaseModel):
    type: Literal["tool_result"]
    tool_call_id: str
    result: ToolResultValue | AgentToolResultValue


class ToolRequest(BaseModel):
    function_name: str
    arguments: dict[str, JsonValue] = Field(default_factory=dict)


class ToolRequestedData(BaseModel):
    type: Literal["tool_requested"]
    tool_call_id: str
    request: ToolRequest


EventData = Annotated[
    MessagesData | ToolRequestedData | ToolResultData, Field(discriminator="type")
]


class ConversationEvent(BaseModel):
    id: str
    session_id: str
    turn_id: str
    created_at: str
    data: EventData


class ConversationEvents(BaseModel):
    events: list[ConversationEvent]
    cursor: str | None = None


async def export_trial_trajectory(
    client: ExoClient,
    conversation: str,
    trial_id: str,
    instruction: str,
    model_name: str,
    destination: Path,
) -> None:
    page = ConversationEvents.model_validate_json(
        await client.read_conversation_events(
            conversation,
            types=["messages", "tool_requested", "tool_result"],
            limit=10_000,
        )
    )
    events = page.events
    if not events:
        raise ValueError(f"conversation {conversation} has no events for {trial_id}")

    turn_ids = list(dict.fromkeys(event.turn_id for event in events))
    trajectory = build_trajectory(
        events=events,
        trial_id=trial_id,
        turn_ids=turn_ids,
        instruction=instruction,
        model_name=model_name,
        conversation=conversation,
        started_at=events[0].created_at,
    )
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = destination.with_suffix(destination.suffix + ".tmp")
    temporary.write_text(json.dumps(trajectory.to_json_dict(), indent=2) + "\n")
    temporary.replace(destination)


def build_trajectory(
    *,
    events: list[ConversationEvent],
    trial_id: str,
    turn_ids: list[str],
    instruction: str,
    model_name: str,
    conversation: str,
    started_at: str,
) -> Trajectory:
    """Map Exo's canonical events to one validated ATIF trajectory."""
    steps = [
        Step(
            step_id=1,
            timestamp=started_at,
            source="user",
            message=instruction,
        )
    ]
    calls: dict[str, Step] = {}
    usages: list[Usage] = []

    for event in events:
        if isinstance(event.data, MessagesData):
            assistant_messages = [
                message
                for message in event.data.messages
                if isinstance(message, AssistantMessage)
            ]
            if not assistant_messages:
                continue

            text: list[str] = []
            reasoning: list[str] = []
            tool_calls: list[ToolCall] = []
            for message in assistant_messages:
                for content in message.content:
                    if isinstance(content, TextContent):
                        if content.text:
                            text.append(content.text)
                    elif isinstance(content, ReasoningContent):
                        if content.text:
                            reasoning.append(content.text)
                    else:
                        tool_calls.append(
                            ToolCall(
                                tool_call_id=content.tool_call_id,
                                function_name=content.tool_name,
                                arguments=content.arguments.value,
                            )
                        )

            metrics = _metrics(event.data.usage)
            step = Step(
                step_id=len(steps) + 1,
                timestamp=event.created_at,
                source="agent",
                model_name=event.data.usage.model if event.data.usage else None,
                message="\n".join(text),
                reasoning_content="\n".join(reasoning) or None,
                tool_calls=tool_calls or None,
                metrics=metrics,
                llm_call_count=1,
            )
            steps.append(step)
            for call in tool_calls:
                calls[call.tool_call_id] = step
            if event.data.usage is not None:
                usages.append(event.data.usage)
            continue

        if isinstance(event.data, ToolRequestedData):
            # Exo's own harness already lists the call inside the assistant
            # message above; the coding-agent harnesses only record it here,
            # so give it a step of its own.
            if event.data.tool_call_id in calls:
                continue
            step = Step(
                step_id=len(steps) + 1,
                timestamp=event.created_at,
                source="agent",
                message="",
                tool_calls=[
                    ToolCall(
                        tool_call_id=event.data.tool_call_id,
                        function_name=event.data.request.function_name,
                        arguments=event.data.request.arguments,
                    )
                ],
            )
            steps.append(step)
            calls[event.data.tool_call_id] = step
            continue

        result = event.data.result
        step = calls.get(event.data.tool_call_id)
        if step is None:
            continue
        if not isinstance(result, ToolResultValue):
            observation = ObservationResult(
                source_call_id=event.data.tool_call_id,
                content=result.text(),
                extra={"ok": result.ok()},
            )
        else:
            observation = ObservationResult(
                source_call_id=event.data.tool_call_id,
                content=(
                    json.dumps(result.value, indent=2)
                    if result.value is not None
                    else result.preview
                ),
                extra={
                    "ok": result.ok,
                    "source": result.source,
                    "tool_name": result.tool_name,
                    "truncated": result.truncated,
                },
            )
        if step.observation is None:
            step.observation = Observation(results=[observation])
        else:
            step.observation.results.append(observation)

    return Trajectory(
        session_id=conversation,
        trajectory_id=trial_id,
        agent=Agent(name="exo", version="unknown", model_name=model_name),
        steps=steps,
        final_metrics=FinalMetrics(
            total_prompt_tokens=sum(usage.prompt_tokens for usage in usages),
            total_completion_tokens=sum(usage.completion_tokens for usage in usages),
            total_cached_tokens=sum(usage.prompt_cached_tokens for usage in usages),
            total_cost_usd=sum(usage.cost_usd for usage in usages),
            total_steps=len(steps),
        ),
        extra={
            "harbor_trial_id": trial_id,
            "exo_agent": conventions.AGENT_SLUG,
            "exo_conversation": conversation,
            "exo_turn_ids": turn_ids,
        },
    )


def _metrics(usage: Usage | None) -> Metrics | None:
    if usage is None:
        return None
    return Metrics(
        prompt_tokens=usage.prompt_tokens,
        completion_tokens=usage.completion_tokens,
        cached_tokens=usage.prompt_cached_tokens,
        cost_usd=usage.cost_usd,
        extra={"reasoning_tokens": usage.completion_reasoning_tokens},
    )
