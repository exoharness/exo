#!/usr/bin/env node

const DEFAULT_PORT = 3939;
const DEFAULT_PATH = "/slack/events";

const explicitUrl = process.argv[2] ?? null;
const publicUrl = explicitUrl ?? (await ngrokPublicUrl());

console.log("Slack app manifest:\n");
console.log(slackManifest());

if (publicUrl) {
  const normalized = publicUrl.replace(/\/+$/, "");
  console.log("\nSlack Event Subscriptions Request URL:\n");
  console.log(`${normalized}${DEFAULT_PATH}`);
  console.log("\nSubscribe to these bot events:\n");
  console.log("app_mention");
  console.log("message.im");
} else {
  console.log("\nNo ngrok URL detected.");
  console.log(`Run: ngrok http ${DEFAULT_PORT}`);
  console.log(
    "Then rerun: pnpm slack:setup https://YOUR-NGROK-HOST.ngrok-free.app",
  );
}

console.log("\nExo secrets:\n");
console.log("exo secret set slack-signing-secret --value '<signing-secret>'");
console.log("exo secret set slack-bot-token --value 'xoxb-...'");

async function ngrokPublicUrl() {
  try {
    const response = await fetch("http://127.0.0.1:4040/api/tunnels");
    if (!response.ok) {
      return null;
    }
    const payload = await response.json();
    if (!payload || !Array.isArray(payload.tunnels)) {
      return null;
    }
    const tunnel = payload.tunnels.find(
      (candidate) =>
        typeof candidate.public_url === "string" &&
        candidate.public_url.startsWith("https://"),
    );
    return tunnel?.public_url ?? null;
  } catch {
    return null;
  }
}

function slackManifest() {
  return `display_information:
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
  token_rotation_enabled: false`;
}
