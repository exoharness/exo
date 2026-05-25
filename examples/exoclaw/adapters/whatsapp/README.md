# WhatsApp Adapter

The WhatsApp adapter is an experimental Exoclaw library adapter implemented as a TypeScript worker using Baileys. It runs as a linked-device client: WhatsApp remains owned by the phone account, and Exoclaw connects as an additional device after QR pairing.

## How It Works

The host adapter runner starts `worker.ts` and passes adapter configuration through `EXO_ADAPTER_CONFIG`. The worker stores Baileys auth state on disk, opens a WhatsApp socket, emits JSONL events on stdout, and receives outbound send commands on stdin.

When WhatsApp asks for pairing, the worker logs an ASCII QR code and emits a lifecycle event named `qr`. After pairing, incoming text messages become Exoclaw adapter message events. Outbound `send_adapter_message` calls send plain text to the target WhatsApp chat id.

## Setup

Use the Exoclaw setup flow:

```bash
examples/exoclaw/scripts/exoclaw-repl fresh --pull-sandbox --setup whatsapp
```

The script watches `.exo/exoclaw-adapters.log`, prints the QR code if it appears, and pauses while you scan it. Scan from WhatsApp using the linked-device flow.

The setup prompt at `setup-prompt.md` asks Exoclaw to create a library adapter similar to:

```json
{
  "name": "whatsapp-dev",
  "source": "library",
  "config": {
    "type": "whatsapp",
    "authDir": null,
    "trigger": "all_messages",
    "allowedChats": null,
    "workerCommand": null
  }
}
```

## Configuration

- `authDir` controls where Baileys stores linked-device credentials. If omitted, the worker uses `.exo/adapters/whatsapp/<adapter-id>/auth` or the host-provided adapter state directory.
- `trigger` is `all_messages` or `contacts_only`.
- `allowedChats` can restrict wakeups to specific WhatsApp chat ids.
- `workerCommand` is transformed by the Exoclaw tool layer when a custom worker command is needed; leave it `null` for the shipped worker.

## Quirks And Gotchas

- Baileys is an unofficial WhatsApp Web client library. It works for testing, but it is inherently more brittle than a supported API.
- If another WhatsApp Web session replaces this device, the worker can disconnect with a conflict/replaced message. Re-pair if needed.
- If the session is logged out, delete the adapter auth directory and pair again.
- WhatsApp sends require the target chat id from the inbound wakeup. Do not guess a phone number as the target.
- The worker currently handles text plus captions on image/video messages. It does not expose rich media documents to Exoclaw yet.
- QR codes and chat ids may appear in `.exo/exoclaw-adapters.log`; treat that log as sensitive local state.
