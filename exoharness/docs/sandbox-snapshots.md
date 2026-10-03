# Sandbox Snapshots

Status: implemented for Docker, Daytona, E2B, Sprites, SmolVM, and
Firecracker. Daytona can additionally restore a Docker snapshot (the
cross-provider "teleport" bridge).

## Summary

A snapshot captures explicitly requested sandbox state so that the sandbox can
later be rewound to that state. Snapshots are taken,
listed, and replayed within an `exo` conversation. They give the user — or in
a later iteration, an executor policy — the ability to time-travel a
sandbox's state without forking the conversation itself.

The earlier model only recorded snapshot _metadata_ (a UUID written to the
event log). This work adds the captured artifact, the persistence layer for
it, and the restore path that actually consumes it.

## What you get

- `ConversationHandle::snapshot_sandbox(id, kind)` actually captures the live
  container's filesystem and persists it.
- `ConversationHandle::start_sandbox(StartSandboxRequest { id, snapshot_id, .. })`
  starts a fresh container whose filesystem is sourced from the snapshot,
  preserving the original sandbox's mounts, network policy, and lifecycle.
- A chat-REPL slash-command surface — `/snapshot`, `/snapshots`, `/rewind <id>`,
  `/teleport <provider>` — that drives the round-trip without leaving the
  conversation.
- **Teleportation**: `start_sandbox` accepts an optional `provider` override,
  so a snapshot taken on one backend can be restored under another. The
  flagship path is local Docker → Daytona: the same sandbox id, resumed
  remotely with its filesystem intact.

## What this is not

- Capture kind is explicit: `SnapshotKind::Filesystem` captures all writable
  filesystems and cold-boots on restore; `SnapshotKind::Full` additionally
  captures memory and execution state. External connections are not preserved.
  Unsupported kinds or writable mounts fail instead of silently losing state.
- **Not a conversation rewind.** The event log, message history, and prior
  tool calls are untouched. Use `conversation fork` to rewind the
  conversation itself.
- **Not yet cross-process.** A snapshot can only be taken of a sandbox that
  is live in the _current_ `exo` process (`running_sandboxes` is per-process).
  See "Known limits" below.

## Model

Snapshots are an interaction between three layers:

```
ConversationHandle             ManagedSandboxHandle           ManagedSandboxBackend
       │                              │                                │
  snapshot_sandbox(id, kind) ──► running_sandboxes.get(id).snapshot(kind) ──┐    │
       │                                                          │    │
       ◄────────── SnapshotPayload { format, bytes } ──────────────┘    │
       │                                                                │
  put_bytes / put_json                                                  │
  (manifest.json + payload.bin)                                         │
                                                                        │
  start_sandbox(req) ─── load manifest + payload ──► acquire_from_snapshot(req, payload)
```

`ConversationHandle` orchestrates: it locates the live handle, asks for a
payload, persists the bytes, updates sandbox metadata, and emits the
`SandboxSnapshotted` event. `ManagedSandboxHandle::snapshot` and
`ManagedSandboxBackend::acquire_from_snapshot` are the backend-specific
methods that produce and consume the bytes.

### SnapshotPayload and SnapshotFormat

```rust
pub struct SnapshotPayload {
    pub format: SnapshotFormat,
    pub bytes: Bytes,
}

pub struct SnapshotFormat(Cow<'static, str>);

impl SnapshotFormat {
    pub const DockerImageTar: Self = Self::from_static("docker-image-tar");
    pub const WorkspaceChunksV1: Self = Self::from_static("workspace-chunks-v1");
}
```

`SnapshotPayload` is opaque to the harness. The `format` identifier is the contract
between producer and consumer: a payload produced by one backend can only be
restored by a backend that declares that format in
`ManagedSandboxBackend::consumable_snapshot_formats`. The harness validates
that capability before dispatch. It
never inspects `bytes` — it just persists them and hands them back on
restore.

`SnapshotFormat` is an open string newtype, like `SandboxProvider`, so adding a
backend or a private format does not require editing a core enum. Portable
formats use shared names; opaque references use backend-namespaced names. A
format name includes a version when its wire representation can evolve.

| Format                          | Producer                          | Consumers                          |
| ------------------------------- | --------------------------------- | ---------------------------------- |
| `docker-image-tar`              | Docker                            | Docker, Daytona                    |
| `daytona-ref`                   | Daytona                           | Daytona                            |
| `e2b-ref`                       | E2B                               | E2B                                |
| `sprites-ref`                   | Sprites                           | Sprites                            |
| `smolvm-machine-pack`           | SmolVM                            | SmolVM                             |
| `firecracker-host-ref`          | Firecracker                       | Firecracker, Firecracker-over-Lima |
| `firecracker-filesystem-ref-v1` | Firecracker                       | Firecracker, Firecracker-over-Lima |
| `workspace-chunks-v1`           | reserved for workspace durability | none yet                           |

## Docker pipeline

`ManagedSandboxHandle::snapshot` (Docker):

1. `ensure_warm_sandbox_ready` — make sure the container exists and is the
   one in the warm cache for this sandbox ID.
2. `docker commit -p <container> exo-snap-<uuid>` — pause the container
   during commit for a consistent filesystem capture, then create a new
   image from its layers.
3. `docker save exo-snap-<uuid>` — export the image as a tarball on stdout;
   capture into `Bytes`.
4. `docker image rm exo-snap-<uuid>` — drop the local image. The canonical
   store of the snapshot lives in exoharness storage, not the docker daemon.

`ManagedSandboxBackend::acquire_from_snapshot` (Docker):

1. The harness validates that the selected backend declares
   `docker-image-tar` as consumable.
2. `docker load < payload.bytes` — load the image back into the local
   daemon; parse stdout to find the assigned image reference (the line
   `Loaded image: <ref>`).
3. Build a fresh `SandboxRequest` with `spec.image` swapped for the loaded
   reference. Mounts, network policy, default workdir, lifecycle, and
   sandbox ID and optional scope are preserved from the original request.
4. Evict any pre-existing warm container for this key (we want a fresh
   container booted from the restored image, not a reuse of whatever was
   running before).
5. `docker run --detach …` with the loaded image — exactly the same path as
   a normal cold-start container, just with a different image.

## On-disk layout

Snapshots live under the conversation directory, alongside other
conversation-scoped artifacts:

```
agents/<agent_id>/conversations/<conversation_id>/snapshots/<snapshot_id>/
├── manifest.json   JSON sidecar (StoredSnapshotManifest)
└── payload.bin     raw blob (docker save tarball for `docker-image-tar`)
```

The manifest schema:

```json
{
  "snapshot_id": "019e5782-7c6b-72a2-b4fa-a81bf56eb37e",
  "sandbox_id": "sandbox-019e5782-2a46-7970-a5bf-62900a2233e8",
  "format": "docker-image-tar",
  "created_at": "2026-05-24T01:03:49.867230008Z",
  "payload_size_bytes": 48498688
}
```

This mirrors the existing artifact layout (sidecar `.json` + `.bin` blob in
a per-id directory). A future migration to chunked or streamed storage
would touch a small surface.

Existing manifests need no migration. Deserialization accepts the old `kind`
field and both legacy enum spellings (`docker_image_tar` and
`DockerImageTar`, with equivalent mappings for every former variant). New
writes always use the `format` field and canonical hyphenated identifier.

The snapshot's existence is also recorded in the conversation event log as
`SandboxSnapshotted { sandbox_id, snapshot_id }`, which is what
`/snapshots` walks to list past snapshots.

## CLI surface

Inside the chat REPL (`exo agent run --agent <agent> --thread <conv>`):

```
/snapshot filesystem [<id>]  capture writable filesystem state
/snapshot full [<id>]        capture filesystems, memory, and execution state
                    each prints the new snapshot id
/snapshots          list snapshots taken in this conversation
/rewind <id>        stop the current sandbox, start a fresh one from the
                    named snapshot
/teleport <provider> snapshot the live sandbox and restore it under another
                    provider (e.g. `/teleport daytona` moves a local Docker
                    sandbox up to Daytona)
/help               show command list
```

There is intentionally no top-level `exo thread snapshot` subcommand
today — see "Known limits" for the cross-invocation gap that makes such
a subcommand useless until it's resolved.

## Executable demo

[`crates/cli/tests/snapshot_round_trip.rs`](../../crates/cli/tests/snapshot_round_trip.rs)
is the canonical, runnable reference for using the snapshot APIs. It drives
the harness library directly (no LLM, no binary spawn) and exercises the same
lifecycle this doc describes. Run it manually with:

```
EXO_TEST_SANDBOX_BACKEND=docker cargo test --package exo \
    --test snapshot_round_trip -- --ignored --nocapture
```

The CI integration workflow runs it on push to `main` against each Linux
matrix cell that supports docker. The test self-skips on cells that don't
(`local-process`) so they don't false-fail.

Two live Daytona analogs (both `#[ignore]`d; they need `DAYTONA_API_KEY`
— snapshots are available in Daytona's shared `us` region — and, for the
teleport, a local docker daemon):

```
# native Daytona snapshot + rewind
cargo test -p exo --test snapshot_round_trip_daytona -- --ignored --nocapture

# teleport: local Docker sandbox snapshotted and resumed on Daytona
cargo test -p exo --test teleport_docker_to_daytona -- --ignored --nocapture
```

## Extending to another sandbox backend

To add snapshot support for a new backend (say, Apple's `container` CLI
when it grows a commit/save flow):

1. Choose a stable format identifier. Reuse an existing portable format when
   the bytes have the same contract; otherwise define a backend-local
   `SnapshotFormat::from_static(...)` constant. No core change is required.
2. Implement `ManagedSandboxHandle::snapshot` for that backend's handle
   type, producing the appropriate `SnapshotPayload`. The Docker version in
   `docker_snapshot_container` is the template — three CLI calls and a
   `Bytes` capture.
3. Return every supported identifier from
   `ManagedSandboxBackend::consumable_snapshot_formats`, then implement
   `acquire_from_snapshot` for those formats. The Docker version is the
   template here — load the bytes, get the loaded image reference, swap
   `request.spec.image`, evict + recreate the warm container.
4. Backends that genuinely can't snapshot (the local-process backend
   today, since there's no isolated filesystem) should return an explicit
   error from both methods rather than silently degrading.

No other layer changes. The conversation orchestration, on-disk layout,
and CLI surface are all backend-agnostic.

## Known limits

### Cross-invocation container adoption

Today each `exo` process maintains its own `running_sandboxes` map. A
container created by one invocation is not adopted by a later one even
though it is still alive on the docker daemon, so snapshots can only be
taken of sandboxes acquired in the current process. That is why the
snapshot/rewind UX lives in the chat REPL (one long-running process holds
the container for the conversation's duration) rather than as standalone
`exo` subcommands.

The fix is well-scoped — on `acquire`, query
`docker ps --filter label=exo.sandbox.key=<key> --filter status=running` and
adopt the existing container if its `exo.sandbox.spec-hash` label matches
the requested spec. Once that lands, `exo thread snapshot` and
`exo thread rewind` become trivial CLI subcommands that just call the
same `ConversationHandle` methods the REPL slash commands use.

### Payload size

`SnapshotPayload::bytes` is a single `Bytes` blob and the harness's
`put_bytes` / `get_bytes` take/return `Vec<u8>`. For the typical
debian-base + small workspace, that is a 30-70 MB blob held in memory
during capture and restore — acceptable but not great. A streamed
producer/consumer interface (`AsyncRead`/`AsyncWrite`) is a clean
follow-up if larger images become routine.

### Snapshot lifecycle

There is no GC. Inline payloads remain until their conversation directory
is deleted. Firecracker captures live in the backend's snapshot namespace
and require explicit deletion, independently of their source allocation.
A future addition could prune unused snapshots or evict by total size.

### Restore semantics

Capture semantics belong to the requested `SnapshotKind`; the format controls
which backend can restore the artifact. Filesystem captures start a fresh
process runtime, while full captures restore memory and execution state.
Backends reject unsupported kinds instead of silently changing these semantics.

## Capture kinds and Firecracker filesystem retention

Both `ManagedSandboxHandle::snapshot(kind)` and
`SnapshotHandle::snapshot_sandbox(id, kind)` require a `SnapshotKind`.
The HTTP request requires `snapshot_kind` (`filesystem` or `full`).
Docker, Daytona, Sprites, and SmolVM support filesystem snapshots; E2B's
current adapter supports full snapshots. Docker and SmolVM reject writable
mounted volumes because their image/pack capture does not include them.
The REPL requires `/snapshot <filesystem|full> [<sandbox-id>]`; the executor
snapshot tool requires a `kind` argument. Teleport requests filesystem captures.

Firecracker supports both kinds. Filesystem capture requires version
1.16.2 or newer. Its filesystem format is
`firecracker-filesystem-ref-v1`, also supported through Lima. It syncs each
writable filesystem, pauses the VM, reflinks the root overlay, workspace,
and resource disks, pins the immutable base image, and atomically publishes
an immutable capture. Applications that need a transaction boundary across
filesystems must quiesce their writes before capture. Capture resumes the source. `suspend(request,
SnapshotKind::Filesystem)` publishes the same capture and kills the VM
while it remains paused, then releases the execution allocation.
Full suspension is currently unsupported.

`acquire_from_snapshot` cold-boots filesystem captures, requiring the same
base-image content and guest mount layout. Missing or truncated disks and
already allocated targets are rejected. A capture outlives the source
allocation and is released explicitly with `delete_snapshot`.

On native Firecracker, `filesystem_snapshot_files` returns immutable base
and disk paths with a lease that prevents deletion during upload.
`import_filesystem_snapshot` publishes verified disk files on another
worker's XFS, returning a new local reference. Disk paths are ordered as
root overlay, optional workspace, then resource mounts in specification
order. The caller must checksum downloaded files before importing them.
The requested base image must still be resolvable on the destination.
Exo does not upload to S3 or publish/fence the service's durable checkpoint.

Filesystem captures have a separate 64 GiB budget based on allocated
blocks, conservatively counting shared extents for each file. They are
retained until explicitly deleted; this budget does not evict checkpoints.

The native round-trip test requires root, KVM, an immutable local image,
and an XFS temporary directory:

```sh
TMPDIR=/var/lib/exo FIRECRACKER_IMAGE=/path/to/rootfs.ext4 \
  cargo test -p exoharness --features firecracker \
  filesystem_snapshot_suspend_restart_and_import_round_trip -- --ignored --nocapture
```
