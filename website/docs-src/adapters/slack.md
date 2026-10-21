---
title: Slack
description: Set up a Slack app using Socket Mode or an HTTP endpoint.
---

<script setup>
import SlackSetupTabs from '../.vitepress/theme/SlackSetupTabs.vue';
</script>

# Slack

Choose how Slack connects to Exo:

- **Socket Mode** is the default and easiest for local setup. Exo connects outward to Slack, so you don't need a public URL or tunnel.
- **HTTP** lets Slack call a public HTTPS endpoint. Hosted providers will require this option.

Both have the same conversation and reply behavior. Pick a tab below for the
setup instructions. Slack also documents the two options:
[Socket Mode](https://docs.slack.dev/apis/events-api/using-socket-mode/) and
[HTTP](https://docs.slack.dev/apis/events-api/using-http-request-urls/).

## Who can use it

Allowed Slack users can use the agent with shared credentials from `global`, even
without their own Exo account or vault. Keep the shared model key there. Review
`global` before enabling the adapter: any usable credentials in it, including your
GitHub token or other API keys, become available to everyone the adapter admits.

To use someone's personal GitHub/MCP credentials, enable `use_sender_vault` and
associate their vault with their email once. Exo resolves users through trusted
identity sources, so the same vault can be used across adapters. A workspace/user-ID
pin is optional for tighter control.
Each turn uses its submitter's credentials, without making those credentials
available to later callers. The conversation's history and files remain shared.

Restrict the bot to the users and channels you want to support, e.g.:

```yaml
adapters:
  - type: slack
    vault: support-slack
    allowed_channels: [C123456]
    allowed_users: [U123456]
    direct_messages: false
```

Channel and user limits both apply. Slack Connect channels are rejected by default,
even when the bot has joined them. DMs from other workspaces in the same Enterprise
Grid organization count as internal when Slack confirms their enterprise ID.
Linking private credentials is optional; see
[Private credentials](#private-credentials).

## Setup

<SlackSetupTabs>
<template #socket>

### 1. Create the app

In [Slack's app settings](https://api.slack.com/apps), choose **Create New App**,
then **From a manifest**, and select your workspace. Use this
[manifest](https://docs.slack.dev/reference/app-manifest/):

```yaml
display_information:
  name: Support analyst
features:
  bot_user:
    display_name: Support analyst
    always_online: true
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - chat:write
      - channels:history
      - channels:read
      - users:read
      - im:history
      - im:read
      - reactions:write
settings:
  socket_mode_enabled: true
  event_subscriptions:
    bot_events:
      - app_mention
      - message.channels
      - message.im
      - channel_shared
      - channel_unshared
  interactivity:
    is_enabled: true
```

### 2. Get the two tokens

#### Bot token (`xoxb-...`)

1. In your app's left sidebar, click **OAuth & Permissions**.
2. At the top of the page, under **OAuth Tokens**, click **Install to Workspace**, then **Allow**.
   If the app is already installed and you changed its scopes, click **Reinstall to Workspace**.
   Otherwise, continue to the next step.
3. Back on **OAuth & Permissions**, find **Bot User OAuth Token** in the **OAuth Tokens** section.
   Click **Copy**. The value starts with `xoxb-`; use it for `SLACK_BOT_TOKEN` below.

The manifest above already supplies the bot's scopes.

#### App token (`xapp-...`)

1. In the left sidebar, click **Basic Information**.
2. Scroll down to **App-Level Tokens**, below the **App Credentials** section.
3. Click **Generate Token and Scopes**. Give the token a name, e.g. `exo`.
4. Click **Add Scope**, select [`connections:write`](https://docs.slack.dev/reference/scopes/connections.write/),
   then click **Generate**. This is the only scope the app token needs.
5. Copy the generated token, which starts with `xapp-`; use it for `SLACK_APP_TOKEN` below.

### 3. Store the credentials

Paste the two token values into these commands, then run them in your shell:

```bash
export SLACK_BOT_TOKEN='xoxb-...'
export SLACK_APP_TOKEN='xapp-...'

exo vault create support-slack
exo vault secret create support-slack bot-token --token-env SLACK_BOT_TOKEN
exo vault secret create support-slack app-token --token-env SLACK_APP_TOKEN
```

### 4. Connect the agent

Add this to the frontmatter of your agent file:

```yaml
adapters:
  - type: slack
    vault: support-slack
```

Run the server:

```bash
exo run support-analyst.md --serve
```

</template>
<template #http>

### 1. Prepare the webhook

Create the adapter's vault:

```bash
exo vault create support-slack
```

Add this to the frontmatter of your agent file:

```yaml
adapters:
  - type: slack
    transport: http
    vault: support-slack
```

For a local deployment, start the server:

```bash
exo run support-analyst.md --serve
```

The server prints its webhook URL, normally
`http://127.0.0.1:4766/exo/adapter/slack`. Publish **only this path** over HTTPS using
the [proxy rule below](#local-http-access). Hosted setup is described in [Hosted providers](#hosted-providers).

The adapter waits for its credentials while the endpoint is available for Slack's
URL verification. It starts accepting messages after you import them.

### 2. Create the app

In [Slack's app settings](https://api.slack.com/apps), choose **Create New App**,
then **From a manifest**, and select your workspace. Replace both Request URLs
below with your public webhook URL. If reusing a Socket Mode app, turn off
**Socket Mode → Enable Socket Mode**; Slack sends events only over WebSockets while it is enabled.

```yaml
display_information:
  name: Support analyst
features:
  bot_user:
    display_name: Support analyst
    always_online: true
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - chat:write
      - channels:history
      - channels:read
      - users:read
      - im:history
      - im:read
      - reactions:write
settings:
  socket_mode_enabled: false
  event_subscriptions:
    request_url: https://exo.example.com/exo/adapter/slack
    bot_events:
      - app_mention
      - message.channels
      - message.im
      - channel_shared
      - channel_unshared
  interactivity:
    is_enabled: true
    request_url: https://exo.example.com/exo/adapter/slack
```

#### Bot token (`xoxb-...`)

1. In your app's left sidebar, click **OAuth & Permissions**.
2. At the top of the page, under **OAuth Tokens**, click **Install to Workspace**, then **Allow**.
   If the app is already installed and you changed its scopes, click **Reinstall to Workspace**.
   Otherwise, continue to the next step.
3. Back on **OAuth & Permissions**, find **Bot User OAuth Token** in the **OAuth Tokens** section.
   Click **Copy**. The value starts with `xoxb-`; use it for `SLACK_BOT_TOKEN` below.

#### Signing secret

1. In the left sidebar, click **Basic Information**.
2. Scroll to **App Credentials** and find **Signing Secret**.
3. Click **Show** and copy the value; use it for `SLACK_SIGNING_SECRET` below.

### 3. Store the credentials

Paste the bot token and signing secret into these commands, then run them in your shell:

```bash
export SLACK_BOT_TOKEN='xoxb-...'
export SLACK_SIGNING_SECRET='...'

exo vault secret create support-slack bot-token --token-env SLACK_BOT_TOKEN
exo vault secret create support-slack signing-secret --token-env SLACK_SIGNING_SECRET
```

For hosted execution, add `--provider PROVIDER` to both commands. Exo picks up the
credentials automatically. HTTP requests are verified using Slack's
[signing secret](https://docs.slack.dev/authentication/verifying-requests-from-slack/).

</template>
</SlackSetupTabs>

## Private credentials

You can skip this section if the shared model key and public resources are enough.
Users without private credentials can still talk to the agent; integrations that
require those credentials won't be available to them.

Enable the submitter's vault for each turn:

```yaml
adapters:
  - type: slack
    vault: support-slack
    use_sender_vault: true
```

To grant shared credentials to every turn from this adapter, select execution vaults:

```yaml
adapters:
  - type: slack
    transport: http
    vault: support-slack
    vaults: [sre]
```

`vault` holds only the Slack transport credentials. `vaults` grants the agent access
to the selected execution vaults, alongside `global`. `use_sender_vault` defaults
to `false`; with `true`, each turn also uses its submitter's associated vault, if
available. It does not attach the sender's vault permanently to the thread.

As the Exo administrator, associate a user's vault with their identity:

```bash
exo vault bind alice --email alice@example.com
```

This association is stored in the provider's identity store and applies across
adapters. Email selects the identity; ownership is saved using its stable account
ID. Add `--provider PROVIDER` for a hosted deployment.

Trust the workspace's directory for email resolution:

```bash
exo identity trust slack --workspace T123456
```

Add `users:read.email` to the app's bot scopes, then reinstall it. The manifests
already include `users:read`. Exo looks up the authenticated Slack sender
and resolves their account through that trusted source. For an Enterprise Grid
workspace, this trusts email claims from members of the whole Grid organization,
including its other workspaces. An email typed into a message does not select a
private vault. Guest and external Slack Connect profiles
aren't automatically trusted for this purpose.

If you want a stricter association, optionally pin the Slack workspace/user ID:

```bash
exo identity bind slack --workspace T123456 --user U123456 --account alice@example.com
```

Inspect these associations with `exo identity list`. To remove a pin, use
`exo identity unbind slack --workspace T123456 --user U123456`. To stop trusting
the directory, use `exo identity untrust slack --workspace T123456`.

Configure local associations before starting the server. While it is running,
manage them through an authenticated HTTP provider using `--provider PROVIDER`.

That pin takes precedence over email lookup and is shared by adapters on the
provider. It isn't required for the email-based path. `allowed_users` independently
controls who can talk to the agent.

When private access is removed, the user can still use shared credentials if the
adapter permits them, in the same conversation. History, files, and data already
fetched stay available to subsequent turns; removing access prevents new use of
the credential, without erasing earlier results. For example, if one participant
checks out a private repository, another participant's turn can read that checkout.

## Hosted providers

::: info Planned
Hosted adapter deployment is not implemented yet. Local Socket Mode and HTTP are available.
:::

Braintrust and Cloudflare providers require `transport: http`. They supply the
public webhook endpoint, so you don't run a local server or configure a tunnel.

With your provider configured, create the vault and agent there, e.g.:

```bash
exo vault create support-slack --provider braintrust
exo agent create support-analyst.md --provider braintrust
```

The agent command returns the webhook URL. It is the provider's base URL followed
by `/agent/AGENT/adapter/ADAPTER_NAME`, e.g. a provider base of
`https://my-worker.workers.dev/exo` gives
`https://my-worker.workers.dev/exo/agent/support-analyst/adapter/slack`.
Use this URL in both fields of the HTTP manifest.

Import the Slack credentials into that provider's vault:

```bash
exo vault secret create support-slack bot-token --token-env SLACK_BOT_TOKEN --provider braintrust
exo vault secret create support-slack signing-secret --token-env SLACK_SIGNING_SECRET --provider braintrust
```

The CLI reads the environment variables on your machine and stores the credentials
on the selected provider. Local vaults do not move with the agent definition. Keep
the shared model key and user/vault associations on the execution provider too.

## Local HTTP access

Slack needs a public HTTPS URL for HTTP delivery. For local execution, forward only
the webhook path. For example, this
[Caddy rule](https://caddyserver.com/docs/caddyfile/directives/handle) forwards Slack
POST requests and returns 404 for other paths:

```text
exo.example.com {
  @slack {
    method POST
    path /exo/adapter/slack
  }
  handle @slack {
    reverse_proxy 127.0.0.1:4766
  }
  handle {
    respond 404
  }
}
```

Local `--serve` without `--auth-file` trusts loopback clients. Publishing the whole
port would expose its thread, turn, and vault APIs. Keep those routes private, or
configure Exo authentication if you also publish the API. The Slack webhook uses
Slack request verification instead of an Exo login.

## Try it

Open the app's **Messages** tab in Slack and send `What is 1+1?`. For a channel,
invite the bot and mention it, e.g. `@Support analyst investigate this ticket`.
Exo adds a 👀 reaction when your message is accepted, then streams a reply.
That reply shows assistant text as it arrives, the tools being called, and whether
Exo is running tools or waiting for the model or your approval. Everything updates
in the same reply, including the final answer. This uses the manifest's existing
`chat:write` scope. A reproduction can take several minutes.
Reply in that Slack thread to continue without mentioning the bot again.

The adapter's credentials are separate from the model credential used by the
agent's threads.

`message.channels` sends the Exo server messages from public channels the bot has
joined, including messages that don't mention it. Exo filters these before creating
turns, but the messages do reach the server. This subscription enables follow-ups
without another mention. See Slack's
[channel event reference](https://docs.slack.dev/reference/events/message.channels/).
For private channels, also add `groups:history`, `groups:read`, and `message.groups`,
reinstall the app, and invite it to the channel.

## Options

| Field | Default | Meaning |
| --- | --- | --- |
| `vault` | Required | Vault containing `bot-token`, plus `app-token` for Socket Mode or `signing-secret` for HTTP. |
| `vaults` | `[]` | Named vaults available to agent turns. The transport `vault` is excluded; hosted providers require shared vaults in the agent’s project. |
| `use_sender_vault` | `false` | Also use the submitter's associated vault for that turn, if available. `global` remains available. History and files stay shared within the conversation. |
| `transport` | `socket` | `socket` or `http`. Braintrust and Cloudflare require explicit `http`. |
| `name` | `slack` | Distinguishes multiple Slack adapters on one agent. |
| `trigger` | `mentions_only` | `mentions_only` accepts mentions, participating thread follow-ups, and DMs. `all_messages` also accepts other channel messages. Both apply user and channel restrictions. |
| `allowed_channels` | Omitted | All eligible channels the bot has joined. A list restricts channel IDs; `[]` accepts no channel input. |
| `allowed_users` | Omitted | All eligible Slack users, with or without private credentials. A list restricts Slack user IDs; `[]` accepts no users. |
| `direct_messages` | `true` | Accept DMs independently of the channel allowlist. |
| `allow_slack_connect` | `false` | Accept Slack Connect conversations, including DMs and group DMs, only when enabled and their IDs are explicitly listed in `allowed_channels`. User restrictions still apply. |

Open the channel's details and copy the channel ID at the bottom. Omit
`allowed_channels` to accept all eligible channels the bot has joined. To disable
channel input while optionally keeping DMs:

```yaml
allowed_channels: []
```

To accept only one channel:

```yaml
allowed_channels: [C123456]
```

Each Slack channel thread maps to one Exo thread, shared by its participants.
Top-level DMs share a thread within that DM conversation; explicit DM reply threads
map to separate Exo threads. With `use_sender_vault: true`, each turn uses the
submitter's vault while retaining the shared history and files. Replies in the
channel are visible to its participants.
The mapping survives server restarts.

Approvals appear as buttons in the originating conversation. Only the author of
the message that started that particular turn can answer them.

The webhook path uses the adapter's name, e.g. `/exo/adapter/support` for
`name: support`. When serving the whole provider with `exo run --serve`, include
the agent as well: `/exo/agent/support-analyst/adapter/slack`.

## Troubleshooting

- "Updates unavailable": malformed progress data stopped delivery after five
  failed attempts without progress. The turn remains saved in Exo. Check the
  server logs for its thread, turn, and decoding error before continuing the
  conversation; this stopped reply is not automatically retried.
- Socket connection fails: check that Socket Mode is enabled and that the app token has `connections:write`.
- Only some Socket Mode messages arrive: check that another server isn't using the same app token. Slack distributes events across active connections; run one server per app token. See [multiple connections](https://docs.slack.dev/apis/events-api/using-socket-mode/#using-multiple-connections).
- HTTP URL won't verify: check that Socket Mode is disabled, the Exo server is running, and the public URL forwards to its webhook path.
- HTTP requests are rejected: check the signing secret and the server clock, and make sure the proxy preserves the request body and Slack signature headers.
- Approval buttons don't work over HTTP: check the Interactivity Request URL.
- No channel response: invite the bot, mention it, and check `allowed_channels`.
- No follow-up response: check the channel event subscription and history scope; reinstall after changing scopes.
- Can't send a DM: enable the app's Messages tab and allow users to send messages to it.
- DM gets no response: subscribe to the `message.im` bot event and grant `im:history` and `im:read`; reinstall after changing scopes. For HTTP, also check that Socket Mode is disabled.
- `missing_scope`: add the scope named in the server log, reinstall the app, and update the vault's `bot-token` if Slack issued a different token.
- Token rejected: check that the credentials belong to the same app and that the app is installed.
- User is rejected: check `allowed_users`, channel/DM restrictions, and Slack Connect settings. A private vault isn't required for shared-only access.
- Private integration is unavailable: enable `use_sender_vault`, then check the trusted identity source, vault ownership, Exo permissions, and credential in the user's vault. If an explicit workspace/user-ID pin exists, check it too.
- Shared channel is ignored: Slack Connect is disabled by default; enabling it also requires an explicit channel allowlist.
