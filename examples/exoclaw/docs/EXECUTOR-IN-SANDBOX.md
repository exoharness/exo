# Running the Exoclaw Executor Inside a Kernel-Owned Sandbox

This document describes a split deployment of Exoclaw in which the **executor**
runs inside a container that the **kernel** owns and tracks, while all durable
state lives in the kernel and is reached over an authenticated HTTP channel.
The end state: the agent runs *in its own sandbox*, can inspect/edit its own
code in that sandbox (`policy_shell`), and does task work in a separate sandbox
(`shell`) — with one component (the kernel) owning and routing every container.

It is the concrete realization of the "executor in its own container" direction;
see `ARCHITECTURE-LEARNINGS.md` for the exploration and rejected alternatives.

---

## 1. Roles and terminology

There is **one `exo` binary**, run in two roles:

| Role | a.k.a. | What it is | Where it runs |
| --- | --- | --- | --- |
| **EH** — exoharness / kernel | "the kernel" | durable state (conversations, events, artifacts, **secrets**, model bindings) **and** sandbox lifecycle/ownership | a host process: `exo serve` |
| **EE** — executor | "the policy" | runs the turn loop, spawns the `node tsx` TS policy (`harness.ts`), makes the model call | **inside** a kernel-owned sandbox container |
| sandboxes | — | the containers EE acts on | docker containers EH creates/tracks |

The TS policy (`examples/exoclaw/harness.ts` + tools) is interpreted by `node tsx`
inside EE; it is never compiled into the binary.

---

## 2. Topology

```
                       HOST
   ┌─ EH: exo serve ───────────────────────────────────────┐
   │   durable state + secrets + model bindings              │
   │   owns + tracks ALL sandboxes (docker backend)          │
   │   bind 172.18.0.1:4766, bearer-token auth               │
   └───▲───────────────▲───────────────────▲────────────────┘
       │ HTTP (authed)  │ docker exec       │ docker exec
       │                │                   │
 ┌─────┴─────────┐  ┌───┴───────────┐   ┌───┴───────────┐
 │ POLICY box P  │  │  (P is also   │   │  WORK box W   │
 │  EE runs here │  │   the target  │   │  shell runs   │
 │  policy_shell │  │   of          │   │  here         │
 │  targets P ───┼──┘   policy_shell)│  │               │
 └───────────────┘                   └───────────────────┘
   exo-policy image                    (task environment)
```

- **EE** is a pure `HttpExoHarness` client: it owns **no** state, **no** secrets,
  and drives **no** docker. Every operation — including `run_in_sandbox` — goes
  to EH.
- **EH** owns the docker backend and every container. It resolves each tool's
  sandbox by container id and `docker exec`s into it.
- `policy_shell` resolves to **P** — the very container EE runs in (self-edit).
- `shell` resolves to **W** — a separate task container.

---

## 3. How a turn flows

```
message (e.g. `exo conversation send AGENT CONV "…"` exec'd in P)
  → EE (in P) runs the turn:
      • fetch agent config / conversation / model binding / secret  ──HTTP──► EH
      • call the model (key obtained from EH's secret store)
      • tool call:
          shell        → run_in_sandbox(W) ──HTTP──► EH → docker exec into W
          policy_shell → run_in_sandbox(P) ──HTTP──► EH → docker exec into P (== EE's box)
  → reply
```

EE holds nothing durable. Kill EE's container and recreate it (even on new code)
pointed at the same EH and it resumes with full state — the state was never in EE.

---

## 4. Authenticated transport

`exo serve` (EH) is the kernel's network surface. Two safeguards:

- It binds **loopback only** by default. A non-loopback bind (so sandboxed
  executors can reach it via the docker bridge gateway) is allowed **only when
  `EXO_SERVE_BEARER_TOKEN` is set**.
- When a token is set, the HTTP server requires `Authorization: Bearer <token>`
  on every `/request`, checked **before** the body is parsed. `/health` stays open.

EE sends the token via `--bearer-env <ENV>` (`HttpExoHarness` → `bearer_auth`).

This is what lets EE — which has no local state and no key — pull its model
secret from EH at turn time over a channel that is never open unauthenticated
off-host.

---

## 5. Two environments: `shell` vs `policy_shell`

| Tool | Resolves to | Purpose | Backend |
| --- | --- | --- | --- |
| `shell` | the **work** container W | run task work | docker (EH) |
| `policy_shell` | the **policy** container P (== EE's own box) | inspect/edit the agent's own code | docker (EH) |

Both route through EH; each is a **distinct container with its own id**. They are
distinguished at the spec level — the policy sandbox spec carries a marker
durable filesystem (`/policy`) so its spec hash differs from the work sandbox and
EH gives it a separate warm container.

> Note: this only works when EH owns the sandboxes (docker backend). If the
> executor were given a `local-process` backend, both tools would collapse into
> the executor's own process environment and `policy_shell` would fail (local-
> process rejects durable filesystems). Keeping sandbox ownership in EH is what
> makes the two-environment model hold.

Verified end to end: EE in container `P`, `policy_shell` → `P` (EE's own box),
`shell` → a different container `W`.

---

## 6. The self-modification loop

Because EE runs **inside** P and `policy_shell` resolves **to** P, `policy_shell`
edits the very code EE is running from. That closes the self-edit loop:

```
EE runs in P  ─┐
               ├─ same container ──► editing via policy_shell changes EE's own code
policy_shell → P ─┘
```

Caveat: the binary is prebuilt and the TS is loaded at process start, so an edit
takes effect on the next EE (re)start, not mid-turn. Turning an edit into a
running change is the next layer (build/restart of EE), not covered here.

---

## 7. Components and configuration

Code (this branch):
- `crates/exoharness/src/http/server.rs` — optional bearer auth on the HTTP server.
- `crates/cli/src/main.rs` — bind guard (`EXO_SERVE_BEARER_TOKEN`), `instantiate_harness`
  (Exoclaw uses the passed exoharness), and the `EXO_REMOTE_SANDBOX` pure-remote path.
- `crates/executor/src/typescript.rs` — `exoclaw_from_exoharness` (Exoclaw harness
  honors `--exoharness-url`).
- `crates/executor/src/policy_sandbox.rs` + `harness_tool.rs` — `policy_shell`
  tool and the policy-sandbox resolver.
- `examples/exoclaw/policy-tools.ts` — the `policy_shell` tool definition.
- `examples/exoclaw/policy-sandbox/Dockerfile` — the `exo-policy` image EE runs in.

Knobs:
- `EXO_SERVE_BEARER_TOKEN` (EH side): enables non-loopback bind + per-request auth.
- `--exoharness-url` + `--bearer-env` (EE side): point EE at EH.
- `EXO_REMOTE_SANDBOX=1` (EE side): make EE a pure remote client so EH owns sandboxes.
- agent `--sandbox-provider docker --sandbox-image exo-policy:dev --networking enabled`:
  so EH creates the policy/work containers from the EE image, on a network that
  reaches EH.

---

## 8. Bringing it up (as prototyped)

1. Build the binary; build the image:
   `docker build -t exo-policy:dev -f examples/exoclaw/policy-sandbox/Dockerfile .`
2. Start EH on the bridge gateway with a token:
   `EXO_SERVE_BEARER_TOKEN=… EXO_SANDBOX_BACKEND=docker exo serve --bind <gateway>:4766`
3. Provision in EH: an agent (`--sandbox-provider docker --sandbox-image exo-policy:dev
   --networking enabled`) + a conversation; the `gpt-*` model binding and its secret
   already live in EH.
4. Bootstrap the policy container P (EH creates it; the prototype triggers this with a
   first `policy_shell` call), then run EE **inside P**:
   `docker exec -e EXO_TOK=… -e EXO_REMOTE_SANDBOX=1 P \
      exo --exoharness-url http://<gateway>:4766 --bearer-env EXO_TOK --harness exoclaw \
      conversation send AGENT CONV "…"` (or `repl`).

---

## 9. Known gaps (prototype, not architecture)

- **Bootstrap is semi-manual.** EH should create P *and* launch EE inside it as one
  orchestrated step; the prototype creates P via a `policy_shell` call and then
  `docker exec`s EE into it.
- **Idle reaping.** EH treats P as a warm sandbox (~5 min idle). The container EE
  lives in can be reaped from under it; a real deploy must pin/keep-alive the box EE
  runs in.
- **Networking is incidental.** EE reaches EH because the sandbox lands on the same
  docker bridge EH is bound to (requires `--networking enabled`). A cleaner setup
  would give EE a dedicated, guaranteed path to EH.
- **Edit→run.** `policy_shell` edits EE's own code, but turning that into a running
  change still needs a rebuild/restart of EE — the self-improvement loop is not
  automated here.
