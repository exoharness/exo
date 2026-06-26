/**
 * Twilio WhatsApp adapter worker (outbound-only).
 *
 * Credentials from exo secrets / env:
 *   TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN, TWILIO_WHATSAPP_FROM
 */

import readline from "node:readline/promises";
import process from "node:process";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";

const config = adapterConfig();
const defaultTo = optionalStringField(config, "defaultTo");
const trigger = optionalStringField(config, "trigger") ?? "all_messages";

const accountSid =
  process.env.TWILIO_ACCOUNT_SID ?? process.env.EXO_TWILIO_ACCOUNT_SID;
const authToken =
  process.env.TWILIO_AUTH_TOKEN ?? process.env.EXO_TWILIO_AUTH_TOKEN;
const fromNumber =
  process.env.TWILIO_WHATSAPP_FROM ?? process.env.EXO_TWILIO_WHATSAPP_FROM;

if (!accountSid || !authToken || !fromNumber) {
  writeWorkerEvent({
    type: "error",
    message:
      "Missing TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN, or TWILIO_WHATSAPP_FROM",
  });
  process.exit(1);
}

writeWorkerEvent({
  type: "connected",
  subject: fromNumber,
  metadata: { provider: "twilio", outboundOnly: true, trigger },
});

process.stderr.write(
  `[whatsapp-adapter] Twilio outbound ready from ${fromNumber}\n`,
);

const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
  terminal: false,
});

rl.on("line", async (line) => {
  let command;
  try {
    command = parseWorkerCommand(JSON.parse(line));
  } catch (err) {
    writeWorkerEvent({
      type: "error",
      message: `Invalid worker command: ${err instanceof Error ? err.message : String(err)}`,
    });
    return;
  }

  const to = command.target ?? defaultTo;
  if (!to) {
    writeWorkerEvent({
      type: "command_nack",
      command_id: command.id,
      message: "No target; set defaultTo in adapter config or pass target",
    });
    return;
  }

  try {
    const sid = await sendTwilioWhatsApp(fromNumber, to, command.text);
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
    writeWorkerEvent({
      type: "lifecycle",
      name: "sent",
      metadata: { sid, to },
    });
  } catch (err) {
    writeWorkerEvent({
      type: "command_nack",
      command_id: command.id,
      message: err instanceof Error ? err.message : String(err),
    });
  }
});

async function sendTwilioWhatsApp(
  from: string,
  to: string,
  body: string,
): Promise<string> {
  const url = `https://api.twilio.com/2010-04-01/Accounts/${accountSid}/Messages.json`;
  const credentials = Buffer.from(`${accountSid}:${authToken}`).toString(
    "base64",
  );
  const params = new URLSearchParams({
    From: formatWhatsApp(from),
    To: formatWhatsApp(to),
    Body: body,
  });

  const res = await fetch(url, {
    method: "POST",
    headers: {
      Authorization: `Basic ${credentials}`,
      "Content-Type": "application/x-www-form-urlencoded",
    },
    body: params.toString(),
  });

  if (!res.ok) {
    throw new Error(`Twilio ${res.status}: ${await res.text()}`);
  }

  const data = (await res.json()) as { sid?: string };
  return data.sid ?? "unknown";
}

function formatWhatsApp(number: string): string {
  const trimmed = number.trim();
  if (trimmed.startsWith("whatsapp:")) return trimmed;
  return `whatsapp:${trimmed}`;
}
