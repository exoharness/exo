# WhatsApp adapter (Twilio)

Outbound-only Twilio WhatsApp adapter for WorkerClaw.

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

This adapter sends outbound messages only.

**Inbound:** Twilio posts incoming messages to a webhook URL you configure in
the Twilio console — not to this sidecar worker. To handle inbound WhatsApp in
exo you need either (a) a host webhook that wakes the adapter conversation, or
(b) the Baileys linked-device worker under `examples/exoclaw/adapters/whatsapp/`
for socket-based inbound inside a worker process. See
[`README.md`](./README.md) for details.
