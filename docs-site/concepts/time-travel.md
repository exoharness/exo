---
title: Time Travel
parent: Concepts
nav_order: 3
---

# Time Travel

At any point in time, the entire state of an agent is defined by the
version of its event log. That single invariant is what makes time travel
possible: you can **rewind or fork from any point in the log**, and every
part of the data model — conversations, artifacts, sandbox state — can be
recreated as of that point.

## Rewind and fork

- **Rewind** returns a conversation to a known-good earlier state, without
  losing secrets, bindings, or the ability to inspect what happened after
  (the log is append-only; history isn't destroyed).
- **Fork** branches a new conversation from an existing one:

```bash
exo conversation fork <agent> <conversation> "Fork Name"
```

The data model supports recreating state as of *any* past event; the CLI
currently exposes forking at the conversation level.

## Sandboxes travel too

Sandboxes can be snapshotted, which writes a snapshot id into the event
log. An executor can snapshot after every action, or let the LLM decide
when. Because snapshots live in the log, rewinding a conversation can also
rewind its sandbox to the matching filesystem state.

## Why this matters for self-modifying agents

An agent that experiments on itself — editing its tools, prompts, or
harness code — needs an undo button that it *cannot* break. Because the
event log lives in the trusted exoharness, below everything the agent can
touch, a failed experiment is always recoverable: rewind the sandbox,
fork from before the change, and the canonical history of what went wrong
is still there to learn from.
