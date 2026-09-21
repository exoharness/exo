# Subscription auth for claude-code and codex harnesses

The `claude-code` and `codex` harnesses (`exoharness/examples/typescript/`)
can authenticate to the model provider using a personal Claude Pro/Max or
ChatGPT subscription instead of an API key billed per token. This is a
first-class `authMode` on the LLM binding (`--auth-mode subscription`),
alongside the existing `--auth-mode api-key` (the default, unchanged
behavior).

Subscription auth does not change anything about the `api-key` path — a
binding registered with `--secret` behaves exactly as it always has.

## Prerequisites

- The sandbox executor image must have the harness binaries installed at the
  paths the harnesses invoke: `/usr/local/bin/claude-code` and `codex` on
  `PATH`. Without these, no turn can run regardless of auth mode.
- `--harness codex` requires the agent's sandbox to have networking enabled,
  since codex makes its own model calls from inside the sandbox:
  `exo agent update <slug> --networking enabled`.
- This has only been verified against the Docker sandbox backend, run
  locally. Subscription auth on a remote/cloud sandbox backend is out of
  scope — those typically require an API key regardless.

## Claude: `claude setup-token`

Claude Code's normal `claude login` is an interactive browser-callback flow
(`localhost:<port>`) that doesn't work headless, and its resulting session
token only lasts ~8h with a refresh flow that isn't reliable outside an
interactive terminal.

Instead, run `claude setup-token` once, on a host with a browser. It opens a
one-time browser authorization and prints a token of the form
`sk-ant-oat-...`, valid for about a year.

1. On the host: `claude setup-token`, complete the browser flow, copy the
   printed token.
2. Register the model binding **without** a key secret:
   ```
   exo model register claude-subscription --model claude-opus-5 \
     --auth-mode subscription
   ```
3. Set `CLAUDE_CODE_OAUTH_TOKEN` in the environment of the `exo` process that
   runs the turn (e.g. inline, or via `--env-file` pointing at a local,
   gitignored dotenv file — never commit it):
   ```
   CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat-..." exo conversation send <agent> <conversation> "..."
   ```

`claudeSandboxBaseEnv` (`exoharness/examples/typescript/claude-code-harness.ts`)
forwards any `CLAUDE_`-prefixed environment variable straight into the
sandbox, so the token reaches the sandboxed `claude-code` process without
exo needing to know its value. Under `--auth-mode subscription`, the harness
fails fast with an actionable error if `CLAUDE_CODE_OAUTH_TOKEN` isn't set —
it never silently runs without credentials, and it never lets a stray
`ANTHROPIC_API_KEY` win by precedence if both happen to be present (that's a
hard error, not a fallback).

## Codex: mounted `auth.json`

The `codex` CLI's own `codex login` (ChatGPT subscription flow) writes
`auth.json` under `$CODEX_HOME`. The codex harness's sandbox boot script only
runs `codex login --with-api-key` when `OPENAI_API_KEY` is set _and_
`auth.json` doesn't already exist — so a pre-existing, mounted `auth.json`
is used as-is, with no key ever touching the sandbox.

1. On a host with a browser, log into codex with your ChatGPT subscription
   (`codex login`, or just use an existing `~/.codex/auth.json` from your
   normal codex usage). Verify it's subscription-based, not API-key:
   ```
   python3 -c "import json; print(json.load(open('~/.codex/auth.json'))['auth_mode'])"
   ```
   should print `chatgpt`, not have a non-null `OPENAI_API_KEY` field.
2. **Do not mount your real `~/.codex` directory.** `codex app-server` needs
   _write_ access to `$CODEX_HOME` (it keeps session/lock state there, even
   just to authenticate) — mounting it read-write would let the sandboxed
   process write into your real host codex profile. Instead, copy just the
   credential into a scratch directory and mount that:
   ```
   mkdir -p /tmp/codex-home-scratch
   cp ~/.codex/auth.json /tmp/codex-home-scratch/auth.json
   chmod 600 /tmp/codex-home-scratch/auth.json
   ```
3. Register the model binding without a key secret. The model must be one
   your ChatGPT plan actually supports — codex rejects some model names
   ("not supported when using Codex with a ChatGPT account") that work fine
   under an API key; check your working `~/.codex/config.toml` for a model
   name you already use successfully.
   ```
   exo model register codex-subscription --model <your-supported-model> \
     --auth-mode subscription
   ```
4. Mount the scratch credential directory into the agent or conversation,
   read-write, as an **internal** mount (see "Credential mount hygiene"
   below):
   ```
   exo conversation mount add <agent> <conversation> --internal --rw \
     /tmp/codex-home-scratch /tmp/exo-codex-home
   ```
   Note the mount must be added at whatever scope the agent's sandbox
   actually uses (`exo agent show <agent>` prints `sandbox_scope`) —
   `exo agent mount add` only applies to `agent`-scoped sandboxes;
   `exo conversation mount add` only applies to that one conversation. A
   mount added at the wrong scope is silently ignored (`exo conversation
mount list` will show `none`), so verify it landed before running a turn.
5. Ensure `OPENAI_API_KEY` is absent from the environment. Under
   `--auth-mode subscription`, the sandbox boot script checks for
   `auth.json` in `CODEX_HOME` before starting `codex app-server` and fails
   fast with an actionable message if it's missing, instead of proceeding
   into a confusing downstream "missing bearer" error from the model API.

## Credential mount hygiene

- Always pass `--internal` on the mount. Today this is enforced by
  construction on the Docker backend rather than by an explicit exo check:
  `snapshot()` there is `docker commit` + `docker save`, and Docker's own
  `commit` only captures the container's writable filesystem layer — bind
  mounts (what every exo mount is) are excluded regardless of `internal`.
  See `docker_snapshot_excludes_internal_mounted_credential_content` in
  `crates/exoharness/src/basic_tests.rs` for a test that proves this
  structurally rather than by reading Docker's docs. This has **not** been
  verified for VM-based backends (firecracker/smolvm), where a snapshot
  captures full disk state and a mount's inclusion depends on how it's
  wired into the guest — don't assume the same guarantee there.
- Never commit a real credential or `auth.json` fixture. If you create test
  fixtures, keep them under a gitignored path (`.env`, `.exo`, and similar
  are already ignored; extend as needed).
- Never let both an API-key secret and a subscription credential
  (`CLAUDE_CODE_OAUTH_TOKEN` / a mounted Codex `auth.json`) apply to the same
  turn. `exo model register` rejects `--secret` together with
  `--auth-mode subscription` at registration time; `claudeSandboxBaseEnv`
  separately fails hard if both are present at runtime (e.g. a leftover
  `CLAUDE_CODE_OAUTH_TOKEN` in the environment alongside an api-key binding)
  rather than silently letting the API key win by precedence.

## Auth-mode diagnostic

Every turn logs a `subscription_auth_diagnostic` custom event reporting which
auth mode was active and — if both a key and a subscription credential were
somehow present — a warning, without ever including credential material. See
`reportAuthDiagnostic` / `authDiagnostic` in
`exoharness/typescript/model-runtime/shared.ts`.
