# Cloud Workstations sandbox backend

A `ManagedSandboxBackend` that runs a DOSSBOT (or any exo) tool-exec on a Google
Cloud Workstation. The harness, executor, and event log stay central/durable;
the workstation is just a sandbox BACKEND exo dispatches `exec`/`start_process`
into over an IAP-tunnelled SSH path.

## Mechanism

`gcloud` is the transport (it owns ADC/IAP auth, tunnelling, host keys), so the
backend stays thin: it shapes argv, runs the process with a bounded timeout, and
maps stdout/stderr/exit-code into a `SandboxCommandOutput`.

| trait method            | gcloud call                                                                                                     |
| ----------------------- | --------------------------------------------------------------------------------------------------------------- |
| `acquire`               | `gcloud workstations start <ws> --project --region --cluster --config` (idempotent: already-running is success) |
| `exec`                  | `gcloud workstations ssh <ws> ... --command="cd <cwd> && <ENV=..> <argv>"`                                      |
| `start_process`         | same `ssh` argv, spawned with piped stdio                                                                       |
| `stop`                  | `gcloud workstations stop <ws> ...` when `stop_on_release` is set, else no-op                                   |
| `snapshot`              | `bail!` (v1 unsupported)                                                                                        |
| `acquire_from_snapshot` | `bail!` (v1 unsupported)                                                                                        |

The remote command string prefixes a `cd` and deterministically-ordered, shell-
quoted env assignments before the shell-quoted argv (ssh runs one remote shell,
so env/cwd have to be baked in). gcloud's tunnel banners land on stderr; remote
stdout is clean.

### Snapshot (future, upstream)

`snapshot`/`acquire_from_snapshot` `bail!` in v1, exactly like the local-process
backend. A Cloud Workstation's persistent state is its home-disk PD; the
documented future path is a new `SnapshotKind` variant (e.g. `GcpPdSnapshot`)
backed by GCP persistent-disk snapshots: `snapshot` -> create-PD-snapshot,
`acquire_from_snapshot` -> restore-PD + start-workstation-off-restored-disk.
That fits exo's closed-enum `SnapshotKind` model and is a separate contribution.

## Configuration

Defaults to the known remoco fleet coords:
`project=remoco-cloud`, `cluster=remoco`, `config=wiley-xl`, `region=us-central1`.
No secret leasing: `gcloud` carries its own auth.

```bash
exo provider configure --provider cloud-workstations \
  --workstation wiley            # required
  # optional overrides:
  # --project remoco-cloud --cluster remoco --workstation-config wiley-xl
  # --region us-central1 --stop-on-release
```

The workstation id may also be supplied per-request via the sandbox image field
(mirrors how e2b overloads `spec.image` as the template id).

## TS wiring note — how `placement:"managed_remote"` selects this backend

(For the autoharness governance-ring / placement layer at
`autoharness/src/exo/placement.ts`. This is the design; the TS side is separate
parallel work — placement.ts currently throws on `managed_remote` as "Phase 2".)

Phase 1's `placement:"local"` drives the exo CLI with `--sandbox-backend
local-process` (the local backends are the only `--sandbox-backend` arg values).
The remote backends — e2b, daytona, and now cloud-workstations — are NOT
selected via `--sandbox-backend`; they are selected by:

1. a one-time `Binding::Sandbox` (`exo provider configure --provider
cloud-workstations --workstation <id>`), persisted in the durable `.exo`
   state, and
2. the per-conversation/agent `--sandbox-provider cloud-workstations`
   preference at send time.

So the `managed_remote` route in placement.ts is the SAME governance ring as
local (placement-agnostic: ring-gate intent -> charter ceiling -> admission),
then a different exo invocation tail:

```ts
// planManagedRemoteRun (Phase 2, parallel TS work):
if (admission.placement !== "managed_remote") throw ...;   // ring already enforces allowedPlacements

// One-time, idempotent (skip if the binding already exists):
//   exo --root <root> provider configure --provider cloud-workstations \
//       --workstation <admission.cellWorkstation ?? "wiley">
//
// Then send on the cloud-workstations provider instead of the local backend:
const command = [
  exoBin, "--root", exoRoot, "--harness", executorModule,
  "--sandbox-provider", "cloud-workstations",   // <- replaces --sandbox-backend local-process
  "conversation", "send", agentSlug, conversationSlug, intent.objective.text,
];
```

Key invariant from the roadmap (P2.2): one intent + one charter + one RunRef span
local <-> managed; only the backend (and thus this last CLI tail) differs. The
executor (`runTurn`) and the event log are unchanged — only the untrusted
tool-exec lands in the cell.
