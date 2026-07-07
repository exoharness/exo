# ExoChat Adapter

The ExoChat adapter connects Exo to the lightweight WebSocket relay under
`website/`. Both the browser and adapter connect outbound to the public relay,
so setup requires no phone number, bot token, or third-party workspace.

## Setup

Use canonical setup:

```bash
scripts/exo.sh fresh --canonical
```

The control script starts the adapter runner, watches `.exo/exo-adapters.log`,
and prints an ExoChat URL. Open that URL in a browser or on your phone to chat
with the agent.

For local relay testing, start the worker in one terminal:

```bash
pnpm dlx wrangler dev --config website/wrangler.jsonc --port 8787
```

Then start Exo against it from the repo root:

```bash
EXO_CHAT_BASE_URL=http://127.0.0.1:8787 scripts/exo.sh fresh --canonical
```

The setup prompt creates a library adapter similar to:

```json
{
  "name": "exochat",
  "source": "library",
  "config": {
    "type": "exochat",
    "baseUrl": null,
    "channelId": null,
    "secret": null
  }
}
```

`baseUrl: null` uses the default hosted ExoChat relay. `channelId` and
`secret` are generated and persisted in the adapter state directory when omitted.

## Content

ExoChat is text-only for now. Inbound browser messages wake Exo as normal
adapter messages, and outbound `send_adapter_message` sends plain text back to
the browser. Attachments are rejected so the hosted relay stays cheap and
predictable. Larger files and rich content can use a later WebRTC/direct path or
object storage backed upload/download endpoints.
