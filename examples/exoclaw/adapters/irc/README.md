# IRC Adapter

The IRC adapter is a built-in Exoclaw adapter implemented as a TypeScript worker. The host adapter runner supervises the worker, passes configuration through `EXO_ADAPTER_CONFIG`, receives JSONL events on stdout, and sends outbound messages by writing JSONL commands to stdin.

## How It Works

On startup, the worker opens a TCP or TLS socket to the configured IRC server, optionally sends `PASS`, then registers with `NICK` and `USER`. After the server sends welcome numeric `001`, the worker joins the configured channel.

The worker handles `PING` with `PONG`, parses `PRIVMSG` lines, and emits Exoclaw message events when the configured trigger policy matches. Outbound `send_adapter_message` calls are converted into `PRIVMSG <channel> :<text>` on the existing IRC connection.

## Setup

Use the Exoclaw setup flow:

```bash
examples/exoclaw/scripts/exoclaw-repl fresh --pull-sandbox --setup irc
```

The setup prompt at `setup-prompt.md` asks Exoclaw to create a built-in adapter similar to:

```json
{
  "name": "libera-exo-test",
  "source": "built_in",
  "config": {
    "type": "irc",
    "server": "irc.libera.chat",
    "port": 6697,
    "tls": true,
    "nick": "exospooky",
    "username": "exospooky",
    "realname": "Exoclaw Test Bot",
    "channel": "#exoclaw",
    "passwordSecretId": null,
    "trigger": "mention"
  }
}
```

Edit the nick and channel before using it on a public network. The default test channel and nick are only examples.

## Configuration

- `server`, `port`, and `tls` select the IRC endpoint.
- `nick`, `username`, and `realname` are used during IRC registration.
- `channel` must start with `#`.
- `passwordSecretId` can be used by the host-side config transform to inject `EXO_IRC_PASSWORD`; leave it `null` for unauthenticated networks.
- `trigger` is either `mention` or `all_messages`.

## Quirks And Gotchas

- IRC nicknames are global per network. If the adapter reports `Nickname is already in use`, pick a different nick or stop the old runner.
- With `trigger: "mention"`, the bot only wakes Exoclaw when a channel message mentions the bot nick.
- IRC has limited formatting and no rich document support. Treat it as a reliable control channel, not a rich UI.
- The worker exits on socket close so the host runner can restart it.
- Outbound messages always go to the configured channel; the `target` from `send_adapter_message` is not required by this worker.
