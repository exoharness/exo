# WhatsApp adapter (Twilio)

Outbound-only Twilio WhatsApp adapter for WorkerClaw.

## What it does today

- **Outbound:** `send_adapter_message` → Twilio REST API → WhatsApp user
- **Inbound:** not implemented in this worker

The worker connects to Twilio, accepts `send_message` commands on stdin, and
returns ack/nack events. It does **not** maintain a socket or HTTP listener for
incoming messages.

## Inbound (not in this worker)

Twilio delivers inbound WhatsApp to an **HTTP webhook** you configure in the
Twilio console — not to a long-lived sidecar process. To wake WorkerClaw on
inbound messages you need a separate ingress path, for example:

1. **Webhook → exo wakeup** — A small HTTP handler receives Twilio POSTs,
   validates the signature, and triggers an adapter conversation wakeup with
   the message text and sender (via your host's exo integration).
2. **Baileys-style worker** — The linked-device approach in
   `examples/exoclaw/adapters/whatsapp/` keeps a WhatsApp Web socket inside the
   worker and emits `message` events on stdout (full bidirectional, unofficial
   API, QR pairing). That is a different integration model than Twilio.

The exo adapter protocol already supports inbound: workers emit
`{ type: "message", target, sender, text, ... }` on stdout and the Rust adapter
runtime records the event and wakes the conversation. The Twilio worker simply
does not emit those events yet.

## Secrets

Provide via exo conversation or agent secrets (or env in the adapter process):

- `TWILIO_ACCOUNT_SID`
- `TWILIO_AUTH_TOKEN`
- `TWILIO_WHATSAPP_FROM`

## Config

```json
{
  "type": "whatsapp",
  "defaultTo": "+15551234567",
  "trigger": "all_messages"
}
```

`defaultTo` is the fallback recipient for outbound sends when no target is
passed. `trigger` is reserved for future inbound filtering; it has no effect
while the worker is outbound-only.

## Outbound example

After creating the adapter, ask WorkerClaw:

```text
Send "hello from WorkerClaw" on WhatsApp adapter <adapter-id> to +15551234567.
```

Or rely on `defaultTo` in config and omit the target in `send_adapter_message`.

## TODOs

- **Twilio inbound webhook (later)** — Optional exo-native path: HTTP handler receives Twilio POSTs, validates signature, emits adapter `message` events / wakes the conversation. Not needed when a host platform already owns ingress (e.g. a Receiver on a webhook service that creates tasks and only uses this worker for outbound `send_adapter_message` during execution).
- **Rich attachments** — Outbound media (image, document) via Twilio; inbound media parsing if inbound is added.
- **`trigger` / `allowedChats`** — Wire config fields once inbound filtering exists.
