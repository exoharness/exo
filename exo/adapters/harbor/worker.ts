import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import process from "node:process";
import readline from "node:readline";
import inputReadline from "node:readline/promises";

import {
  adapterConfig,
  optionalStringField,
  parseWorkerCommand,
  writeWorkerEvent,
} from "../protocol";
import {
  composeHarborPrompt,
  defaultSocketPath,
  parseHarborRequest,
  parseHarborResponse,
  requestTarget,
  type HarborRequest,
  type HarborResponse,
} from "./harbor";

// Bridges the host-side Harbor integration to Exo's conversation wakeups.
//
// Inbound: a JSON line on the unix socket becomes a `message` worker event,
// which the harness turns into a conversation wakeup.
// Outbound: Exo calls send_adapter_message with the target we supplied; that
// arrives here on stdin and is written back to the waiting socket client.
//
// Unlike the agent-cli adapter, a Harbor exchange can run for many minutes and
// can outlive this process: Exo may rebuild and restart itself mid-task, which
// drains the adapter runner and starts a fresh worker. So completed responses
// are persisted, and a reconnecting client replays the answer rather than
// waking Exo a second time.

const config = adapterConfig();
const socketPath =
  optionalStringField(config, "socketPath") ?? defaultSocketPath();
const stateDir = process.env.EXO_ADAPTER_STATE_DIR ?? ".";
// Needed in the wakeup text: send_adapter_message takes an adapterId, and a
// prompt that omits it invites the model to invent one.
const adapterId = process.env.EXO_ADAPTER_ID ?? "";

type Pending = {
  request: HarborRequest;
  sockets: Set<net.Socket>;
};

// One entry per in-flight exchange, keyed by requestTarget().
const pending = new Map<string, Pending>();

fs.mkdirSync(path.dirname(socketPath), { recursive: true });
fs.mkdirSync(stateDir, { recursive: true });
// The runner guarantees a single worker per adapter, so a leftover socket file
// is always stale and safe to remove.
fs.rmSync(socketPath, { force: true });

const server = net.createServer((socket) => {
  socket.setEncoding("utf8");

  const lines = readline.createInterface({
    input: socket,
    crlfDelay: Number.POSITIVE_INFINITY,
  });

  lines.on("line", (line) => {
    if (line.trim().length === 0) {
      return;
    }
    let request: HarborRequest;
    try {
      request = parseHarborRequest(JSON.parse(line));
    } catch (error) {
      sendToClient(socket, { type: "error", message: errorText(error) });
      return;
    }

    const target = requestTarget(request);

    // Already answered: replay instead of waking Exo again. This is what makes
    // a host-side reconnect after a worker restart safe.
    const completed = readCompletedResponse(target);
    if (completed) {
      sendToClient(socket, { type: "response", event: completed });
      return;
    }

    // Already in flight: attach this socket to the existing exchange rather
    // than starting a second one.
    const existing = pending.get(target);
    if (existing) {
      existing.sockets.add(socket);
      return;
    }

    pending.set(target, { request, sockets: new Set([socket]) });
    writeWorkerEvent({
      type: "message",
      target,
      sender: "harbor",
      text: composeHarborPrompt(request, adapterId, target),
      message_id: request.message_id,
      metadata: {
        trial_id: request.trial_id,
        task_name: request.task_name,
        conversation_id: request.conversation_id,
      },
    });
  });

  socket.on("error", () => {
    // A client that vanishes mid-exchange is normal; cleanup happens on close.
  });

  socket.on("close", () => {
    // Drop the socket but keep the exchange. Exo is still working, and a
    // reconnecting client should still be able to collect the answer.
    for (const entry of pending.values()) {
      entry.sockets.delete(socket);
    }
  });
});

server.on("error", (error) => {
  writeWorkerEvent({
    type: "error",
    message: `harbor listener error: ${error.message}`,
  });
  process.exit(1);
});

server.listen(socketPath, () => {
  fs.chmodSync(socketPath, 0o600);
  process.stderr.write(`[harbor-adapter] listening on ${socketPath}\n`);
  writeWorkerEvent({
    type: "connected",
    subject: socketPath,
    metadata: { socketPath },
  });
});

process.on("exit", () => {
  fs.rmSync(socketPath, { force: true });
});

// Outbound: Exo's send_adapter_message calls arrive here.
const input = inputReadline.createInterface({
  input: process.stdin,
  crlfDelay: Number.POSITIVE_INFINITY,
});

for await (const line of input) {
  if (line.trim().length === 0) {
    continue;
  }
  let commandId: string | null = null;
  try {
    const command = parseWorkerCommand(JSON.parse(line));
    commandId = command.id;

    const target = command.target;
    if (!target) {
      throw new Error(
        "harbor send_message requires the target from the inbound message",
      );
    }

    const entry = pending.get(target);
    if (!entry) {
      // Either already answered — ack so Exo is not told its reply failed —
      // or a stale trial, which is a genuine error.
      if (readCompletedResponse(target)) {
        writeWorkerEvent({ type: "command_ack", command_id: command.id });
        continue;
      }
      throw new Error(`harbor target ${target} is not awaiting a response`);
    }

    const response = parseHarborResponse(command.text, entry.request);
    // Persist before delivering, so a crash between the two cannot lose an
    // answer Exo has already produced.
    writeCompletedResponse(target, response);
    pending.delete(target);
    for (const socket of entry.sockets) {
      sendToClient(socket, { type: "response", event: response });
    }
    writeWorkerEvent({ type: "command_ack", command_id: command.id });
  } catch (error) {
    // Nack rather than throw: a malformed reply should come back to Exo as a
    // tool error it can correct, not kill the adapter mid-trial.
    const message = errorText(error);
    if (commandId !== null) {
      writeWorkerEvent({
        type: "command_nack",
        command_id: commandId,
        message,
      });
    } else {
      writeWorkerEvent({ type: "error", message });
    }
  }
}

function responsePath(target: string): string {
  const digest = crypto.createHash("sha256").update(target).digest("hex");
  return path.join(stateDir, `response-${digest}.json`);
}

function readCompletedResponse(target: string): HarborResponse | null {
  try {
    return JSON.parse(fs.readFileSync(responsePath(target), "utf8"));
  } catch {
    return null;
  }
}

function writeCompletedResponse(target: string, response: HarborResponse) {
  const destination = responsePath(target);
  const temporary = `${destination}.tmp`;
  fs.writeFileSync(temporary, JSON.stringify(response));
  fs.renameSync(temporary, destination);
}

function sendToClient(socket: net.Socket, payload: object): void {
  socket.write(`${JSON.stringify(payload)}\n`);
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
