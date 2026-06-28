# Slack Adapter

The Slack adapter is a local, non-central library adapter. Slack sends events to a public ngrok URL, ngrok forwards them to a local Exo worker, and the worker replies directly to Slack with the workspace-owned bot token.

The MVP is text-only and defaults to public `app_mention` events plus Slack DMs. The app manifest enables the App Home messages tab so Slack users can DM the bot. There is no Exo-hosted Slack app and no Exo relay service in the message path.

## Setup

### Chat Setup

Start Exoclaw normally:

```bash
scripts/exo.sh
```

Then ask the agent:

```text
Help me set up Slack.
```

The agent will walk you through creating the Slack app, storing secrets, creating the adapter, starting ngrok, and enabling Slack Event Subscriptions.

This startup shortcut is still available, but optional:

```bash
scripts/exo.sh --setup slack
```

You do not need to run `pnpm slack:setup` for the chat-driven flow.

### Manual Setup Details

#### 1. Create a Slack App

1. Open the Slack app dashboard: <https://api.slack.com/apps>.
2. Click **Create New App** > **From an app manifest**.
3. Pick your workspace.
4. Paste the manifest shown by the interactive wizard. If you are doing this without the wizard, print the same manifest with:

   ```bash
   pnpm slack:setup
   ```

5. Open **Basic Information** and copy the **Signing Secret**.
6. Store it locally:

   ```bash
   exo secret set slack-signing-secret --value '<signing-secret>'
   ```

7. Open **OAuth & Permissions**, click **Install to Workspace**, approve the install, and copy the **Bot User OAuth Token** starting with `xoxb-`.
8. Store it locally:

   ```bash
   exo secret set slack-bot-token --value 'xoxb-...'
   ```

#### 2. Store the Secrets in Exo

```bash
exo secret set slack-signing-secret --value '<signing-secret>'
exo secret set slack-bot-token --value 'xoxb-...'
```

#### 3. Create the Exo Adapter

During interactive setup, the agent creates the adapter after you confirm the Slack secrets are stored. You can also send the setup prompt while starting a fresh local Exoclaw agent:

```bash
scripts/exo.sh \
  --agent exospooky \
  --conversation dev \
  --setup slack
```

The setup prompt creates a library adapter similar to:

```json
{
  "name": "slack-dev",
  "source": "library",
  "config": {
    "type": "slack",
    "botTokenSecretId": "slack-bot-token",
    "signingSecretId": "slack-signing-secret",
    "port": 3939,
    "path": "/slack/events",
    "defaultChannelId": null,
    "trigger": "mentions_only",
    "allowedChannels": null,
    "allowBots": false,
    "threadReplies": true,
    "conversationScope": "target"
  }
}
```

#### 4. Start ngrok

In a separate terminal:

```bash
ngrok http 3939
```

The interactive wizard asks for your ngrok HTTPS origin and gives you the exact Slack Request URL. If you are doing this manually, you can also print it with:

```bash
pnpm slack:setup https://YOUR-NGROK-HOST.ngrok-free.app
```

#### 5. Enable Slack Events

In the Slack app dashboard:

1. Open **Event Subscriptions** and enable events.
2. Set **Request URL** to the URL printed by `pnpm slack:setup`, usually `https://...ngrok-free.app/slack/events`.
3. Under **Subscribe to bot events**, add `app_mention` and `message.im`.
4. In **App Home**, make sure the **Messages Tab** is enabled and not read-only. The manifest sets this automatically for new apps.
5. Save changes and reinstall the app if Slack asks.
6. Invite the bot to a channel with `/invite @YourBot`.

#### 6. Test It

Mention the bot in Slack:

```text
@YourBot hello from Slack
```

The adapter wakes Exo with a target shaped as `CHANNEL_ID:THREAD_TS` for public mentions. Use that target for public replies; the worker posts back into the same Slack thread. Slack DMs use synthetic targets shaped as `dm:USER_ID`; the worker opens the DM with `conversations.open` and sends there.

## Configuration

- `botTokenSecretId` is the Exo secret name or id containing the Slack Bot User OAuth Token.
- `signingSecretId` is the Exo secret name or id containing the Slack Signing Secret.
- `port` and `path` define the local event endpoint. The default setup uses `3939` and `/slack/events`.
- `trigger` is `mentions_only` or `all_messages`. With the default setup, `mentions_only` still wakes on Slack DMs from the `message.im` event.
- `allowedChannels` optionally restricts inbound wakeups to Slack channel ids.
- `threadReplies` makes inbound targets `CHANNEL_ID:THREAD_TS`, so replies post into the same Slack thread.
- `conversationScope: "target"` creates a separate Exo conversation for each Slack target.

For all channel messages, set `trigger: "all_messages"` and add the Slack bot event `message.channels` plus the `channels:history` bot scope. Private channel support needs the matching Slack scopes and events.
