# Signal Adapter

The Signal adapter is an experimental Exoclaw library adapter implemented as a TypeScript worker around `signal-cli`. It assumes you already have a real Signal account on a phone, have set a Signal username, and want Exoclaw to run as a linked device.

## How It Works

The host adapter runner starts `worker.ts` and passes adapter configuration through `EXO_ADAPTER_CONFIG`. The worker stores `signal-cli` state on disk, discovers or links a local account, starts `signal-cli jsonRpc --receive-mode=on-connection` for inbound messages, emits JSONL events on stdout, and receives outbound send commands on stdin.

If `account` is `null`, the worker first runs `signal-cli listAccounts`. If an account already exists in the configured state directory, it reuses it. Otherwise it runs `signal-cli link -n <deviceName>`, prints a QR code, waits for linking to complete, discovers the account, and then starts JSON-RPC receive mode.

Incoming Signal messages become Exoclaw adapter message events. Outbound `send_adapter_message` calls send through the same `signal-cli jsonRpc` process with either a recipient or a group id, depending on the target format.

## Setup

Install the JVM `signal-cli` distribution and Java. The native `signal-cli` binary can receive messages but has failed outbound sends in testing.

```bash
brew install openjdk
```

The current local setup expects the JVM script at:

```text
/Users/martin/.local/bin/signal-cli-jvm
```

Run the setup with Java on `PATH`:

```bash
PATH="/opt/homebrew/opt/openjdk/bin:$PATH" \
examples/exoclaw/scripts/exoclaw-repl fresh --pull-sandbox --setup signal
```

The script watches `.exo/exoclaw-adapters.log`, prints the linked-device QR code if it appears, and pauses while you scan it from Signal: Settings > Linked devices > Link new device.

The setup prompt at `setup-prompt.md` asks Exoclaw to create a library adapter similar to:

```json
{
  "name": "signal-dev",
  "source": "library",
  "config": {
    "type": "signal",
    "account": null,
    "deviceName": "Exoclaw",
    "signalCliCommand": ["/Users/martin/.local/bin/signal-cli-jvm"],
    "configDir": null,
    "trigger": "all_messages",
    "allowedContacts": null
  }
}
```

## Configuration

- `account` is the local Signal account identifier for `signal-cli -a`. Use `null` for first-time setup or automatic account discovery.
- `deviceName` is shown in Signal's linked-device list during pairing.
- `signalCliCommand` is the command array used to run `signal-cli`. Use `["/Users/martin/.local/bin/signal-cli-jvm"]` for the tested JVM script, or `null` to use `signal-cli` from `PATH`.
- `configDir` controls where `signal-cli` stores linked-device state. If omitted, the worker uses `.exo/adapters/signal/<adapter-id>/signal-cli` or the host-provided adapter state directory.
- `trigger` is `all_messages` or `contacts_only`.
- `allowedContacts` can restrict wakeups to specific sender identifiers.

## Targets

Signal outbound sends require a target. Use the `target` from the inbound wakeup when replying. For direct messages, supported target forms include:

- Signal usernames with the `u:` prefix, such as `u:example.01`.
- Phone numbers, such as `+16505551212`.
- Signal UUIDs from inbound events.
- `ACI:` or `PNI:` identifiers.

Long opaque non-recipient strings are treated as group ids.

## Quirks And Gotchas

- The Homebrew/native `signal-cli` binary may fail outbound sends with `NETWORK_FAILURE` and an error mentioning `IdentityKeyDeserializer has no default (no arg) constructor`. Use the JVM distribution instead.
- Java must be visible on `PATH` for the JVM `signal-cli` script. On Homebrew macOS, prefix commands with `PATH="/opt/homebrew/opt/openjdk/bin:$PATH"` or add that path to your shell profile.
- If linking succeeds but later setup keeps asking for QR codes, check that the same `configDir` is being reused.
- Signal group support depends on targets surfaced by incoming group messages. Prefer replying to the inbound target instead of inventing group ids.
- QR codes, account ids, phone numbers, and message routing metadata may appear in `.exo/exoclaw-adapters.log`; treat that log as sensitive local state.
