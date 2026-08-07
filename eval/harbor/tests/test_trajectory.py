import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock

from exo_harbor.trajectory import (
    ConversationEvents,
    build_trajectory,
    export_trial_trajectory,
)


TRIAL_ID = "514afdba-87e0-43cc-a3ac-86422ade3cf8"
TURN_ID = "019fde0e-d687-7be3-8771-d9f5a4309e0d"


def event_page(events: list[dict]) -> str:
    return json.dumps({"events": events, "cursor": events[-1]["id"] if events else None})


def messages_event(
    event_id: str,
    messages: list[dict],
    *,
    turn_id: str = TURN_ID,
    usage: dict | None = None,
) -> dict:
    data = {"type": "messages", "messages": messages, "response_id": None}
    if usage is not None:
        data["usage"] = usage
    return {
        "id": event_id,
        "thread_id": "conversation-id",
        "session_id": "session-id",
        "turn_id": turn_id,
        "created_at": "2026-08-07T21:09:02.215Z",
        "data": data,
    }


class TrajectoryTest(unittest.IsolatedAsyncioTestCase):
    async def test_export_selects_trial_turn_and_writes_atif(self) -> None:
        other = messages_event(
            "event-1",
            [{"role": "user", "content": "Harbor started trial `other-trial`"}],
            turn_id="other-turn",
        )
        start = messages_event(
            "event-2",
            [
                {
                    "role": "user",
                    "content": f"Harbor started trial `{TRIAL_ID}` for task `task`.",
                }
            ],
        )
        assistant = messages_event(
            "event-3",
            [
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "reasoning",
                            "text": "Inspect the output.",
                            "encrypted_content": "ciphertext",
                        },
                        {
                            "type": "tool_call",
                            "tool_call_id": "call-1",
                            "tool_name": "shell",
                            "arguments": {
                                "type": "valid",
                                "value": {"command": "run tests"},
                            },
                        },
                    ],
                    "id": "response-item",
                }
            ],
            usage={
                "model": "gpt-5.5-2026-04-23",
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "prompt_cached_tokens": 80,
                "completion_reasoning_tokens": 10,
                "cost_usd": 0.02,
            },
        )
        tool_result = {
            "id": "event-4",
            "thread_id": "conversation-id",
            "session_id": "session-id",
            "turn_id": TURN_ID,
            "created_at": "2026-08-07T21:09:03.215Z",
            "data": {
                "type": "tool_result",
                "tool_call_id": "call-1",
                "result": {
                    "ok": True,
                    "preview": '{"stdout":"Smy Smy"}',
                    "source": "built_in",
                    "toolName": "shell",
                    "truncated": False,
                    "value": {"exit_code": 0, "stdout": "Smy Smy", "stderr": ""},
                },
            },
        }
        client = AsyncMock()
        client.model = "gpt-5.5"
        client.read_conversation_events.side_effect = [
            event_page([other, start]),
            event_page([start, assistant, tool_result]),
        ]

        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "agent" / "trajectory.json"
            await export_trial_trajectory(
                client,
                "harbor-shared",
                TRIAL_ID,
                "Do the task",
                destination,
            )
            output = json.loads(destination.read_text())

        self.assertEqual(output["schema_version"], "ATIF-v1.7")
        self.assertEqual(output["trajectory_id"], TRIAL_ID)
        self.assertEqual(output["session_id"], TURN_ID)
        self.assertEqual(output["steps"][0]["message"], "Do the task")
        self.assertEqual(len(output["steps"]), 2)
        agent_step = output["steps"][1]
        self.assertEqual(agent_step["reasoning_content"], "Inspect the output.")
        self.assertEqual(agent_step["tool_calls"][0]["function_name"], "shell")
        self.assertIn("Smy Smy", agent_step["observation"]["results"][0]["content"])
        self.assertEqual(output["final_metrics"]["total_prompt_tokens"], 100)
        self.assertNotIn("ciphertext", json.dumps(output))
        self.assertEqual(client.read_conversation_events.await_count, 2)
        self.assertEqual(
            client.read_conversation_events.await_args_list[1].kwargs["turn_id"],
            TURN_ID,
        )

    def test_build_trajectory_replaces_adapter_envelope_with_instruction(self) -> None:
        events = ConversationEvents.model_validate_json(
            event_page(
                [
                    messages_event(
                        "event-1",
                        [{"role": "user", "content": "adapter protocol envelope"}],
                    )
                ]
            )
        ).events

        trajectory = build_trajectory(
            events=events,
            trial_id=TRIAL_ID,
            turn_id=TURN_ID,
            instruction="Do the task",
            model_name="gpt-5.5",
            conversation="harbor-shared",
            started_at="2026-08-07T21:09:02.215Z",
        )

        self.assertEqual(len(trajectory.steps), 1)
        self.assertEqual(trajectory.steps[0].message, "Do the task")


if __name__ == "__main__":
    unittest.main()
