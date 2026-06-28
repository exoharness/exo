Set up a Slack adapter for local testing with an interactive wizard.

Do not create the adapter immediately. First walk the user through Slack app creation and local secret setup. Ask one short question at a time, wait for the user to confirm each step, and never ask the user to paste the Slack bot token or signing secret into chat.

Use this flow:

1. Show the Slack app manifest below and ask the user to create a Slack app from it at `https://api.slack.com/apps`.
2. After the user confirms the app is created, explain that Slack shows the Signing Secret immediately on the app's Basic Information page. Ask the user to copy the Signing Secret and store it locally with:

   ```bash
   exo secret set slack-signing-secret --value '<signing-secret>'
   ```

   Tell the user not to paste the value into chat. Ask them to reply `signing secret stored` when done.

3. Then tell the user to open **OAuth & Permissions** in the Slack app sidebar, click **Install to Workspace**, approve the install, copy the **Bot User OAuth Token** that starts with `xoxb-`, and store it locally with:

   ```bash
   exo secret set slack-bot-token --value 'xoxb-...'
   ```

   Tell the user not to paste the token into chat. Ask them to reply `bot token stored` when done.

4. After the user confirms both secrets were stored, create a library Slack adapter if one does not already exist for this conversation. Use these settings:

- name: `slack-dev`
- source: `library`
- type: `slack`
- botTokenSecretId: `slack-bot-token`
- signingSecretId: `slack-signing-secret`
- port: `3939`
- path: `/slack/events`
- defaultChannelId: `null`
- trigger: `mentions_only`
- allowedChannels: `null`
- allowBots: `false`
- threadReplies: `true`
- conversationScope: `target`

5. After creating or confirming the adapter, ask the user to start ngrok in a host terminal:

   ```bash
   ngrok http 3939
   ```

6. Ask the user to paste the ngrok HTTPS origin, such as `https://example.ngrok-free.app`. Reply with the Slack Event Subscriptions Request URL by appending `/slack/events`.
7. Tell the user to enable Event Subscriptions in Slack, paste that Request URL, subscribe to the bot events `app_mention` and `message.im`, then open **App Home** and make sure the **Messages Tab** is enabled and not read-only. Save changes, reinstall if Slack asks, invite the bot to a channel with `/invite @Exo`, mention the bot in a channel, and DM the bot to test both paths.

When Slack messages wake the agent, Slack targets work like this:

- Public channel/thread replies use the inbound target, usually `CHANNEL_ID:THREAD_TS`.
- Direct-message replies use `dm:USER_ID`.
- For "DM me when you are uncomfortable answering" behavior, send a brief safe public response first, then optionally DM a safe alternative or clarification. Do not use DM to provide forbidden content privately.

Slack app manifest:

```yaml
display_information:
  name: Exo Local
features:
  app_home:
    home_tab_enabled: false
    messages_tab_enabled: true
    messages_tab_read_only_enabled: false
  bot_user:
    display_name: Exo
    always_online: false
oauth_config:
  scopes:
    bot:
      - app_mentions:read
      - chat:write
      - im:history
      - im:write
settings:
  org_deploy_enabled: false
  socket_mode_enabled: false
  token_rotation_enabled: false
```

If the user says the app and secrets already exist, skip directly to adapter creation. If adapter creation reports a missing Slack token or signing secret, tell the user to run the relevant `exo secret set ... --value ...` command above and then continue from adapter creation.
