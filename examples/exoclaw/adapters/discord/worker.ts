import fs from "node:fs/promises";
import readline from "node:readline/promises";

import {
  ChannelType,
  Client,
  GatewayIntentBits,
  Partials,
  type Message,
  type MessageCreateOptions,
} from "discord.js";

import {
  type AdapterAttachment,
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

const SEND_TIMEOUT_MS = 60_000;

const config = adapterConfig();
const tokenEnv =
  optionalStringField(config, "tokenEnv") ?? "EXO_DISCORD_BOT_TOKEN";
const token = process.env[tokenEnv];
if (!token) {
  throw new Error(`Discord bot token missing from ${tokenEnv}`);
}
const trigger = optionalStringField(config, "trigger") ?? "mentions_only";
const defaultChannelId = optionalStringField(config, "defaultChannelId");
const allowedChannels = stringArrayOrNull(config.allowedChannels);
if (trigger !== "all_messages" && trigger !== "mentions_only") {
  throw new Error("Discord trigger must be all_messages or mentions_only");
}

const client = new Client({
  intents: [
    GatewayIntentBits.Guilds,
    GatewayIntentBits.GuildMessages,
    GatewayIntentBits.DirectMessages,
    GatewayIntentBits.MessageContent,
  ],
  partials: [Partials.Channel],
});

client.on("error", (error) => {
  writeWorkerEvent({ type: "error", message: error.message });
});

client.once("ready", () => {
  writeWorkerEvent({
    type: "connected",
    subject: client.user?.id ?? null,
    metadata: {
      username: client.user?.tag ?? null,
    },
  });
});

client.on("shardDisconnect", (event) => {
  writeWorkerEvent({
    type: "disconnected",
    reason: event.reason || String(event.code),
  });
});

client.on("messageCreate", (message) => {
  if (message.author.bot || message.author.id === client.user?.id) {
    return;
  }
  if (!shouldTrigger(message)) {
    return;
  }
  writeWorkerEvent({
    type: "message",
    target: message.channelId,
    sender: message.author.id,
    text: message.content,
    message_id: message.id,
    metadata: {
      authorUsername: message.author.tag,
      channelId: message.channelId,
      guildId: message.guildId,
      channelType: message.channel.type,
    },
  });
});

await client.login(token);

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    const target = command.target ?? defaultChannelId;
    if (!target) {
      throw new Error(
        "Discord send_message requires a target channel id or configured defaultChannelId",
      );
    }
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_starting",
      metadata: {
        target,
        attachmentCount: command.attachments.length,
      },
    });
    await sendDiscordMessage(target, {
      content: command.text,
      files: await discordAttachmentFiles(command.attachments),
    });
    writeWorkerEvent({
      type: "lifecycle",
      name: "send_result",
      metadata: {
        target,
        attachmentCount: command.attachments.length,
      },
    });
  } catch (error) {
    writeWorkerEvent({
      type: "error",
      message: error instanceof Error ? error.message : String(error),
    });
  }
}

async function sendDiscordMessage(
  target: string,
  options: MessageCreateOptions,
): Promise<void> {
  const channel = await client.channels.fetch(target);
  if (!isSendableChannel(channel)) {
    throw new Error(`Discord target ${target} cannot send messages`);
  }
  let timeout: NodeJS.Timeout | null = null;
  try {
    await Promise.race([
      channel.send(options),
      new Promise<never>((_, reject) => {
        timeout = setTimeout(() => {
          reject(
            new Error(
              `Discord send_message timed out after ${SEND_TIMEOUT_MS}ms`,
            ),
          );
        }, SEND_TIMEOUT_MS);
      }),
    ]);
  } finally {
    if (timeout !== null) {
      clearTimeout(timeout);
    }
  }
}

type SendableChannel = {
  send(options: MessageCreateOptions): Promise<unknown>;
};

function isSendableChannel(channel: unknown): channel is SendableChannel {
  if (!channel || typeof channel !== "object" || !("send" in channel)) {
    return false;
  }
  return typeof (channel as { send?: unknown }).send === "function";
}

async function discordAttachmentFiles(
  attachments: AdapterAttachment[],
): Promise<NonNullable<MessageCreateOptions["files"]>> {
  return Promise.all(
    attachments.map(async (attachment) => ({
      attachment: await attachmentBytes(attachment),
      name: attachment.fileName ?? fileNameForAttachment(attachment),
      description: attachment.mimeType ?? undefined,
    })),
  );
}

async function attachmentBytes(attachment: AdapterAttachment): Promise<Buffer> {
  if (attachment.path) {
    return fs.readFile(attachment.path);
  }
  if (attachment.url) {
    const response = await fetch(attachment.url);
    if (!response.ok) {
      throw new Error(
        `Discord attachment URL fetch failed with ${response.status}`,
      );
    }
    return Buffer.from(await response.arrayBuffer());
  }
  if (attachment.data) {
    return Buffer.from(base64Payload(attachment.data), "base64");
  }
  throw new Error("Discord attachment requires path, url, or data");
}

function shouldTrigger(message: Message): boolean {
  if (
    allowedChannels !== null &&
    !allowedChannels.includes(message.channelId)
  ) {
    return false;
  }
  if (message.channel.type === ChannelType.DM) {
    return true;
  }
  const botId = client.user?.id;
  if (trigger === "all_messages") {
    return true;
  }
  return botId !== undefined && message.mentions.users.has(botId);
}

function fileNameForAttachment(attachment: AdapterAttachment): string {
  switch (attachment.kind) {
    case "image":
      return "image";
    case "video":
      return "video";
    case "audio":
      return "audio";
    case "document":
      return "document";
  }
}

function base64Payload(data: string): string {
  const dataUrlSeparator = data.indexOf(",");
  if (data.startsWith("data:") && dataUrlSeparator !== -1) {
    return data.slice(dataUrlSeparator + 1);
  }
  return data;
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (
    Array.isArray(value) &&
    value.every((item): item is string => typeof item === "string")
  ) {
    return value;
  }
  throw new Error(
    "Discord allowedChannels must be null or an array of strings",
  );
}
