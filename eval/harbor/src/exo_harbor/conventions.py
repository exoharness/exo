"""Shared constants between the plugin and the per-trial agents.

Harbor builds the plugin and each ExoAgent independently and never introduces
them, so whatever the plugin creates in on_job_start has to be rediscoverable
by every new agent instance.

This used to be a session descriptor file the plugin wrote and the agents read.
It is not needed. Every field it would have carried is either already an --ak
the agent holds, or derivable:

* the Exo agent — resolve_agent_handle (crates/executor/src/harness_helpers.rs)
  accepts a UUID *or a slug*, so a fixed slug both sides know is enough and no
  generated id ever has to be passed around
* the socket — a convention under exo_root
* exo_root / exo_bin / conversation_mode — already agent kwargs

What the file was genuinely good for was proving on_job_start had run at all: a
job launched with --agent and no --plugin would otherwise derive everything,
find a plausible Exo, and produce a fully scored run with no feedback loop.
That check is better served by probing the socket (see ExoAgent.setup) — a
live socket proves the adapter is up now, whereas a file only proves it was
written at some point and can be stale from a previous run in a reused
exo_root.
"""

from __future__ import annotations

from pathlib import Path

# Slug for the Exo agent the plugin creates and every trial addresses. Fixed
# rather than generated precisely so no id needs passing between them.
#
# Scoped by exo_root, not globally: two jobs on one host get separate roots and
# therefore separate agents, so a fixed slug cannot collide.
AGENT_SLUG = "harbor-eval"

# Conversation slug used in shared mode. per_task mode derives one per trial
# from the Harbor trial name.
SHARED_CONVERSATION_SLUG = "harbor-shared"


def socket_path(exo_root: Path) -> Path:
    """Where the Harbor adapter listens for this job.

    Under exo_root rather than the adapter's ~/.exo/harbor.sock default, so
    concurrent jobs on one host do not fight over one socket.
    """
    return exo_root / "harbor.sock"


def conversation_slug(trial_name: str) -> str:
    """Conversation slug for a trial in per_task mode."""
    # TODO: normalize trial_name into a slug-safe string.
    raise NotImplementedError
