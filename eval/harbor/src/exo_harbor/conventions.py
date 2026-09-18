from __future__ import annotations

AGENT_SLUG = "harbor-eval"
HARNESS_MODULE = "exo/harness.ts"


def trial_conversation_slug(trial_id: str) -> str:
    return f"trial-{trial_id}"


# Key the agent writes into Harbor's AgentContext.metadata so a trial's
# result.json names the Exo conversation that produced it.
CONVERSATION_METADATA_KEY = "exo_conversation_id"

