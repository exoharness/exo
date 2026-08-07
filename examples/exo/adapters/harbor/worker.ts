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
  expectedResponseType,
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
// Unlike the agent-cli adapter, a Harbor exchange can take many minutes — Exo
// works a whole task before replying — so the client connection is held open
// for the duration and the host side owns the timeout.

const config = adapterConfig();
const socketPath =
  optionalStringField(config, "socketPath") ?? defaultSocketPath();

type Pending = {
  request: HarborRequest;
  sockets: Set<net.Socket>;
};

// One entry per in-flight exchange, keyed by requestTarget().
const pending = new Map<string, Pending>();

fs.mkdirSync(path.dirname(socketPath), { recursive: true });
// The runner guarantees a single worker per adapter, so a leftover socket
// file is always stale and safe to remove.
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
    // TODO: parseHarborRequest(JSON.parse(line)).
    //
    // Then, before waking Exo:
    //
    // 1. If this target already has a persisted response, replay it and
    //    return. The host side retries on reconnect, and a retry must not
    //    wake Exo a second time for work it already finished.
    //
    // 2. If this target is already pending, attach this socket to the
    //    existing entry rather than starting a second exchange.
    //
    // 3. Otherwise record it pending and emit:
    //      writeWorkerEvent({
    //        type: "message",
    //        target: requestTarget(request),
    //        text: composeHarborPrompt(request, target),
    //        message_id: request.message_id,
    //        metadata: { trial_id, conversation_id, sandbox_id },
    //      })
  });

  socket.on("close", () => {
    // TODO: drop this socket from any pending entry, but keep the entry
    // itself — Exo is still working and a reconnect should still collect the
    // answer.
  });
});

server.listen(socketPath, () => {
  fs.chmodSync(socketPath, 0o600);
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
  // TODO: parseWorkerCommand(JSON.parse(line)), then:
  //
  // 1. Require command.target — Exo must echo the target it was given. It is
  //    the only thing tying a reply to an exchange.
  // 2. Look it up in `pending`. Not found => either already answered (check
  //    the persisted response and ack) or a stale trial (nack).
  // 3. parseHarborResponse(command.text, entry.request) — wrong type or
  //    wrong trial_id is a nack, not a delivery.
  // 4. Persist the response before writing it out, so a crash between the
  //    two does not lose it, then write to every attached socket, delete the
  //    pending entry, and command_ack.
  //
  // Nack rather than throw: a malformed reply should come back to Exo as a
  // tool error it can correct, not kill the adapter mid-trial.
}

function persistedResponsePath(target: string): string {
  // TODO: hash the target into the adapter state dir. Completed responses
  // outlive the process so retries after a restart stay idempotent.
  throw new Error("not implemented");
}
