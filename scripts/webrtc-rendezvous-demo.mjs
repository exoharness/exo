#!/usr/bin/env node

import crypto from "node:crypto";
import process from "node:process";
import qrcode from "qrcode-terminal";

const baseUrl = normalizeBaseUrl(
  process.argv[2] ?? process.env.EXO_CHAT_BASE_URL ?? "https://exoharness.ai",
);
const channelId = randomBase64url(18);
const secret = randomBase64url(32);

const phoneUrl = `${baseUrl}/chat/s/${channelId}#k=${secret}`;
const agentUrl = `${baseUrl}/chat/agent/${channelId}#k=${secret}`;

console.log("");
console.log("Open this agent peer URL on your computer:");
console.log(agentUrl);
console.log("");
console.log("Scan this QR code on your phone:");
console.log(phoneUrl);
console.log("");
qrcode.generate(phoneUrl, { small: true });
console.log("");
console.log("Keep the computer tab open, then send messages from either side.");
console.log("");

function randomBase64url(bytes) {
  return crypto.randomBytes(bytes).toString("base64url");
}

function normalizeBaseUrl(value) {
  const url = new URL(value);
  url.hash = "";
  url.search = "";
  return url.toString().replace(/\/$/, "");
}
