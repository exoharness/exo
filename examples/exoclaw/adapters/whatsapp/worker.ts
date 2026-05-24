import fs from "node:fs/promises";
import readline from "node:readline/promises";

import makeWASocket, {
  DisconnectReason,
  fetchLatestBaileysVersion,
  useMultiFileAuthState,
  type WAMessage,
} from "@whiskeysockets/baileys";
import type { ILogger } from "@whiskeysockets/baileys/lib/Utils/logger.js";
import qrcodeTerminal from "qrcode-terminal";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

const config = adapterConfig();
const trigger = optionalStringField(config, "trigger") ?? "all_messages";
const allowedChats = stringArrayOrNull(config.allowedChats);
if (trigger !== "all_messages" && trigger !== "contacts_only") {
  throw new Error("WhatsApp trigger must be all_messages or contacts_only");
}
const authDir =
  optionalStringField(config, "authDir") ??
  (process.env.EXO_ADAPTER_STATE_DIR === undefined
    ? `.exo/adapters/whatsapp/${process.env.EXO_ADAPTER_ID ?? "default"}/auth`
    : `${process.env.EXO_ADAPTER_STATE_DIR}/auth`);

const logger: ILogger = {
  level: "silent",
  child() {
    return logger;
  },
  trace() {},
  debug() {},
  info() {},
  warn(value) {
    process.stderr.write(`[whatsapp-adapter] ${formatLogValue(value)}\n`);
  },
  error(value) {
    process.stderr.write(`[whatsapp-adapter] ${formatLogValue(value)}\n`);
  },
};

await fs.mkdir(authDir, { recursive: true });

const { state, saveCreds } = await useMultiFileAuthState(authDir);
const { version } = await fetchLatestBaileysVersion();
const socket = makeWASocket({
  auth: state,
  logger,
  printQRInTerminal: false,
  syncFullHistory: false,
  version,
});

socket.ev.on("creds.update", saveCreds);

socket.ev.on("connection.update", (update) => {
  if (update.qr) {
    qrcodeTerminal.generate(update.qr, { small: true }, (qr) => {
      process.stderr.write(
        `\n[whatsapp-adapter] Scan this QR with WhatsApp:\n${qr}\n`,
      );
    });
    writeWorkerEvent({
      type: "lifecycle",
      name: "qr",
      metadata: { qr: update.qr },
    });
  }
  if (update.connection === "open") {
    writeWorkerEvent({
      type: "connected",
      subject: socket.user?.id ?? null,
    });
  }
  if (update.connection === "close") {
    const statusCode = statusCodeFromError(update.lastDisconnect?.error);
    writeWorkerEvent({
      type: "disconnected",
      reason: statusCode === null ? null : String(statusCode),
    });
    if (statusCode === DisconnectReason.loggedOut) {
      writeWorkerEvent({
        type: "error",
        message: `WhatsApp session logged out; delete ${authDir} and pair again`,
      });
      process.exit(1);
    }
    process.exit(1);
  }
});

socket.ev.on("messages.upsert", (event) => {
  if (event.type !== "notify") {
    return;
  }
  for (const message of event.messages) {
    const inbound = inboundMessage(message);
    if (inbound === null) {
      continue;
    }
    if (!shouldTrigger(inbound.target)) {
      continue;
    }
    writeWorkerEvent(inbound);
  }
});

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
    if (!command.target) {
      throw new Error("WhatsApp send_message requires a target chat id");
    }
    await socket.sendMessage(command.target, { text: command.text });
  } catch (error) {
    writeWorkerEvent({
      type: "error",
      message: error instanceof Error ? error.message : String(error),
    });
  }
}

function inboundMessage(message: WAMessage) {
  if (message.key.fromMe) {
    return null;
  }
  const chatId = message.key.remoteJid;
  if (!chatId) {
    return null;
  }
  const text = messageText(message);
  if (text === null || text.length === 0) {
    return null;
  }
  return {
    type: "message" as const,
    target: chatId,
    sender: message.key.participant ?? message.key.remoteJid ?? null,
    text,
    message_id: message.key.id ?? null,
  };
}

function messageText(message: WAMessage): string | null {
  const content = message.message;
  if (!content) {
    return null;
  }
  if (content.conversation) {
    return content.conversation;
  }
  if (content.extendedTextMessage?.text) {
    return content.extendedTextMessage.text;
  }
  if (content.imageMessage?.caption) {
    return content.imageMessage.caption;
  }
  if (content.videoMessage?.caption) {
    return content.videoMessage.caption;
  }
  return null;
}

function shouldTrigger(chatId: string): boolean {
  if (allowedChats !== null && !allowedChats.includes(chatId)) {
    return false;
  }
  if (trigger === "contacts_only") {
    return !chatId.endsWith("@g.us");
  }
  return true;
}

function stringArrayOrNull(value: unknown): string[] | null {
  if (value === null || value === undefined) {
    return null;
  }
  if (Array.isArray(value) && value.every((item) => typeof item === "string")) {
    return value;
  }
  throw new Error("WhatsApp allowedChats must be null or an array of strings");
}

function statusCodeFromError(error: unknown): number | null {
  if (!isRecord(error)) {
    return null;
  }
  const output = error.output;
  if (!isRecord(output) || typeof output.statusCode !== "number") {
    return null;
  }
  return output.statusCode;
}

function formatLogValue(value: unknown): string {
  if (value instanceof Error) {
    return value.stack ?? value.message;
  }
  if (typeof value === "string") {
    return value;
  }
  return JSON.stringify(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
