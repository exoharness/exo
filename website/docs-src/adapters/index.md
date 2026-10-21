---
title: Adapters
description: Connect an agent to Slack.
---

# Adapters

::: info Under construction
Slack is implemented for local servers. Hosted deployment is planned.
:::

Adapters let you talk to an agent from a messaging app. They submit messages to
Exo threads and send the agent's responses back to the same conversation. You can
use the same agent from chat, the terminal, or an HTTP client.

Add an adapter to the agent's frontmatter:

```yaml
adapters:
  - type: slack
    vault: support-slack
```

Then serve the agent:

```bash
exo run support-analyst.md --serve
```

Exo maintains the connection while the server is running. Slack conversations map
to persistent threads, so participants' follow-ups retain the same history and filesystem.
Replies are delivered automatically; the agent doesn't need a tool or special
instructions to send them.

`name` is optional and defaults to the adapter's type. Give adapters distinct
names if you connect two accounts of the same type:

```yaml
adapters:
  - type: slack
    name: support
    vault: support-slack
  - type: slack
    name: internal
    vault: internal-slack
```

## Setup

- [Slack](slack): a Slack app using Socket Mode or a public HTTP endpoint.

Each page describes the required credentials, configuration, and how chats map
to threads. The first implementation covers text messages; media support follows
separately.

## Credentials

Bot tokens belong in a dedicated vault selected by the adapter's `vault` field.
Exo reads that vault to connect the adapter, without attaching it to agent threads.
Slack turns use `global`. Set `use_sender_vault: true` to also use the submitter's
associated vault for each turn. The adapter's connection vault stays separate.
See [Vaults](../architecture/vaults) for those credentials.

## Identity

Slack users allowed by the adapter can use shared credentials from `global`,
without having a private vault. Review `global` before enabling an adapter:
everything usable in it is available to the users you admit.

To enable personal credentials, set `use_sender_vault: true` and associate a private
vault with the user's email once. Adapters resolve the user through the provider's
trusted identity sources.
Optional workspace/user-ID pins give tighter control. Put a shared model key in
`global` and private integrations in the user's vault. Channel/user restrictions
control agent access independently of private credential ownership.

One Slack conversation maps to one Exo thread, even when several users participate.
Credentials are selected per turn; history, artifacts, and files remain shared.
Data fetched using one person's credentials can therefore be used in later turns.
Replies in a channel remain visible to that channel's participants. Other transports also need verified sender
identity before they can use private vaults.

## Hosted providers

Hosted Braintrust and Cloudflare providers require Slack's `transport: http` and
supply the webhook URL. Create the agent and its adapter vault on that provider;
local vaults are not uploaded with the definition. See
[Slack hosting](slack#hosted-providers) for the URL and credential commands.

## Running the server

`exo run FILE_OR_AGENT --serve` starts that agent's adapters alongside its HTTP API.
`exo run --serve` serves the whole local provider. Keep the provider's state when
restarting so conversation mappings survive.

Removing an adapter from the definition stops its connection and preserves its
Exo threads. A connection failure is shown in server logs, and Exo reconnects.
Accepted input and pending replies are persisted, so delivery can resume after a
restart. Retries can still duplicate an external reply if the messaging provider
doesn't support idempotent sends.
