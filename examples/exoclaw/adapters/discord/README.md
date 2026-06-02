# Discord Adapter

The Discord adapter is an experimental Exoclaw library adapter implemented as a TypeScript worker using `discord.js`. It logs in as a Discord bot, emits inbound Discord messages as adapter wakeups, and sends explicit outbound messages through `send_adapter_message`.

## Setup

### 1. Create a Discord Bot

1. Open the Discord Developer Portal: <https://discord.com/developers/applications>.
2. Click **New Application**, give it a name, and open the new application.
3. Open **Bot** in the left sidebar.
4. Click **Reset Token** or **View Token**, then copy the bot token. Keep this token private.
5. In **Privileged Gateway Intents**, enable **Message Content Intent**. This is required for Exo to read message text.

### 2. Invite the Bot to a Server

1. Open **OAuth2** > **URL Generator** in the Developer Portal.
2. Under **Scopes**, select `bot`.
3. Under **Bot Permissions**, select at least:
   - `View Channels`
   - `Send Messages`
   - `Read Message History`
   - `Attach Files` if you want Exo to send attachments
4. Copy the generated URL, open it in a browser, and add the bot to your Discord server.
5. In Discord, make sure the bot can see the target channel. Copy the channel id for testing:
   - Enable Discord developer mode in **User Settings** > **Advanced** > **Developer Mode**.
   - Right-click the target channel and choose **Copy Channel ID**.

### 3. Store the Bot Token in Exo

Export the token locally and store it as an Exo secret:

```bash
export DISCORD_BOT_TOKEN="..."
exo secret set discord-bot-token --env DISCORD_BOT_TOKEN
```

The setup prompt below expects the secret name to be `discord-bot-token`.

### 4. Create the Exoclaw Adapter

Run the Exoclaw setup prompt:

```bash
examples/exoclaw/scripts/exoclaw-repl --setup discord
```

If you are setting up a fresh local Exoclaw agent, use the same flags you normally use for your agent/conversation, for example:

```bash
examples/exoclaw/scripts/exoclaw-repl \
  --agent exospooky \
  --conversation dev \
  --setup discord
```

The setup prompt at `setup-prompt.md` asks Exoclaw to create a library adapter similar to:

```json
{
  "name": "discord-dev",
  "source": "library",
  "config": {
    "type": "discord",
    "botTokenSecretId": "discord-bot-token",
    "defaultChannelId": null,
    "trigger": "mentions_only",
    "allowedChannels": null
  }
}
```

Use `defaultChannelId` when you want outbound messages to go to one channel by default. Otherwise, pass the copied Discord channel id as `target` when calling `send_adapter_message`.

### 5. Test It

Ask Exoclaw to send a Discord message with the adapter id returned by setup:

```text
Send "hello from exo" to Discord using adapter <adapter-id> and target <channel-id>.
```

To test inbound wakeups with the default `mentions_only` trigger, mention the bot in the channel:

```text
@YourBot hello exo
```

For `all_messages`, every message in allowed channels can wake the Exoclaw conversation.

## Configuration

- `botTokenSecretId` is the Exoclaw secret name or id containing the Discord bot token.
- `defaultChannelId` is used when `send_adapter_message` is called with `target: null`.
- `trigger` is either `mentions_only` or `all_messages`. Direct messages always trigger.
- `allowedChannels` optionally restricts inbound wakeups to specific Discord channel ids.

Outbound messages support text plus the shared adapter attachment forms: staged `path`, HTTPS `url`, or base64/data URL `data`.

## Rich Attachments

Discord supports outbound image, video, audio, and document attachments through `send_adapter_message`. Prefer `sandboxPath` for files created by shell commands in the Exoclaw sandbox:

```json
{
  "adapterId": "<discord-adapter-id>",
  "target": "<discord-channel-id>",
  "text": "Here is the generated file.",
  "attachments": [
    {
      "kind": "document",
      "path": null,
      "url": null,
      "data": null,
      "sandboxPath": "/tmp/report.txt",
      "mimeType": "text/plain",
      "fileName": "report.txt"
    }
  ]
}
```

For files already visible on the host, use `path`. For remote media, use an HTTPS `url`. For small inline payloads, use base64 `data` or a data URL.
