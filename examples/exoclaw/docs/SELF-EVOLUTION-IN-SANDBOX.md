# Proposal: In-Container Self-Evolution (with Rust recompile)

Status: proposal. Builds on `EXECUTOR-IN-SANDBOX.md` (the split where the
executor **EE** runs inside a kernel-owned sandbox **P**, with state in the
kernel **EH** over an authed channel).

Goal: let the agent change its **own code inside P**, **rebuild** (TS reload or
Rust recompile), **switch** to the new code, and — critically — **snapshot
before** and **auto-roll-back** if the new code fails to run. This is the
exoclaw guardian self-modification loop (`build` → restart) plus the
auto-rollback the old world never had (SELF-CONTROL §8 gap).

---

## 1. The loop

The rollback **baseline** is always the last *verified-good* snapshot — produced
by the previous successful evolve (or by bootstrap for the first one). The agent
never snapshots manually; `evolve` owns it and only snapshots a state that has
passed the health-check. You never snapshot before an edit, and never snapshot a
broken state.

```
(baseline = last verified-good snapshot: from the previous successful evolve, or bootstrap's baseline_0)
1. policy_shell: edit code in P            (TS and/or crates/ Rust)
2. evolve_policy({ rebuild })              (hand off to the supervisor; current turn ends)
   ── supervisor, OUTSIDE P ──────────────────────────────────────────────
   2a. if rebuild: build in P (cargo)      build fails → rewind(baseline) + restart EE; report; stop
   2b. switch EE to the new code           (see §4)
   2c. health-check: probe turn in P       fails → rewind(baseline) + restart EE; report
   2d. PASS → accept; snapshot P → this becomes the new baseline (captures the accepted change)
```

Bootstrap seeds `baseline_0` (snapshot the pristine, verified-running P). Each
accepted evolve advances the baseline to a new known-good snapshot; each failed
evolve rewinds to the current baseline. `target/` (the warm build cache) rides
along in each good baseline, so incremental builds stay fast and a rollback
reverts to a one-version-back warm cache.

The safety invariant carries over from the split: the **editing turn runs the
old code** (EE's process already loaded it); only the **probe turn runs the new
code**. So a broken change can never brick EE mid-turn — it fails the probe and
is rolled back to the baseline.

---

## 2. Key property: snapshot/rewind is the universal rollback

Because P's code **and** its built binary live in P's writable layer
(`/home/worker/exo/...` incl. `target/debug/exo`), an EH `snapshot_sandbox(P)`
captures **both source and binary**, and `rewind_sandbox(P, snap)` restores
both. So the *same* rollback primitive covers TS edits **and** Rust recompiles —
no separate git path (unlike the old mount world, where code was host-backed and
git was the rollback).

Corollary — **snapshots are the durable record of self-evolution.** A P
recreated from the *base image* loses accepted changes; P must be recreated from
the **latest-good snapshot**. Rollback = rewind to an earlier snapshot. The
agent's evolved "self" is its latest snapshot.

---

## 3. Components

### 3a. Policy image (path B): toolchain + crates + warm cache
Extend the `exo-policy` image so a `cargo build` can run **inside P**:
- add the Rust toolchain (pinned to the repo's `rust-toolchain`),
- add `crates/`, `Cargo.toml`, `Cargo.lock` (today we copy only `typescript/` +
  `examples/` + the prebuilt binary),
- `RUN cargo build -p exo` at image-build time to **warm `target/`**, so
  in-P incremental rebuilds touch only changed crates.

Cost: a much larger image (Rust toolchain + crate deps + warm `target/`, GBs).
Mitigate with a shared cargo-cache volume and/or multi-stage build. Path A
(TS-only) does **not** need this image.

### 3b. Reuse the guardian's build, run in P
`examples/exoclaw/scripts/exoclaw-service-guardian build` already does
`cargo build -p exo`. Run **that script inside P** (P has the script, crates, and
toolchain) — literally the same `build_all`, executed in the sandbox instead of
the host. This is the "use exoclaw's tooling" answer for B: the build step is
reused; only *where* it runs changes (host → P, because the editable code now
lives in P).

### 3c. The policy supervisor (outside P) — the new control wrapper
Mirrors `exoclaw-control` (which supervised the REPL child and restarted it on a
marker, from outside the REPL). A host-side process that:
- **bootstraps**: ensure P exists (via EH `create_sandbox`), launch EE in P —
  this also closes the bootstrap gap from `EXECUTOR-IN-SANDBOX.md`;
- **evolves**: on an `evolve_policy` request, runs §1 steps 3a–3d from *outside*
  P (build, restart EE, health-check, rewind-on-fail), then reports the outcome
  back (wakes EE).

It must live outside P for the same reason `exoclaw-control` lived outside the
REPL: a process can't restart/rewind the container it runs in.

### 3d. `evolve_policy` tool (model-facing)
Analogous to `guardian_action`. Parameters: `rebuild: bool` (cargo build needed,
i.e. Rust changed), `note: string`. It records the request + the baseline
snapshot id, hands off to the supervisor **deferred** (so the current turn
finishes — exactly like `guardian_action`'s deferred restart), and reports that
evolution was scheduled. After the supervisor finishes, EE is woken with
"accepted" or "rolled back: <reason>".

### 3e. `Policy` snapshot scope (EH)
Wire a `Policy` variant into `snapshot_sandbox` / `rewind_sandbox` (today: `agent`
/ `conversation`) whose resolver is the policy sandbox P (reusing
`ensure_policy_sandbox`). So the agent can `snapshot_sandbox(scope: policy)` and
`rewind_sandbox(scope: policy, snapshotId)`.

### 3f. Health-check
A probe turn the supervisor runs in P on the new binary, e.g.
`exo conversation send <agent> <probe-conv> "reply READY"`. Healthy =
exit 0 + `READY` in the reply. Failure = crash / nonzero / wrong output.

---

## 4. "Switch to the new code"
- **TS change:** no build. In the send/wakeup model each turn is a fresh `exo`
  process that loads the TS fresh, so the **probe turn already runs the new TS** —
  switch is automatic. (A long-running REPL would need the EE process cycled.)
- **Rust change:** after `cargo build` in P writes a new `target/debug/exo`, the
  **next `exo` process in P uses the new binary** — so the probe turn runs the new
  binary. Again automatic in the send model; cycle the process for a long-running
  EE.

So "recompile and switch" = `cargo build` in P + the next (probe) turn picks up
the new binary. No special swap step.

---

## 5. Failure handling (two rollback triggers)
- **Build failure** (cargo errors in P): `cargo` leaves the previous binary
  intact, but the *source* now has the bad edit → rewind to `snap_good` (restores
  source + binary), report "build failed."
- **Runtime failure** (build OK, new binary/TS crashes or the probe fails):
  rewind to `snap_good` + restart EE on the restored code, report "runtime check
  failed."

Both roll back to the last known-good snapshot — the auto-rollback the old
guardian lacked.

---

## 6. Costs, risks, boundaries
- **Image weight / build time** (§3a): Rust toolchain + crates + warm `target/`.
  Incremental in-P builds are seconds–minutes for a touched executor crate.
- **Idle reaping:** EH treats P as a warm sandbox (~5 min idle). The supervisor
  must keep P alive across build/restart, and recreate P from the **latest-good
  snapshot** (not the base image) so accepted changes survive.
- **Snapshot consistency:** snapshot when EE is idle (between turns) to avoid a
  torn writable layer; EE being fresh-per-turn makes this easy.
- **Kernel boundary (the ceiling):** building in P rebuilds the **executor**
  binary, so the agent can add/Change *executor-side* Rust (new tool match arms,
  executor behavior). The **kernel** (`exoharness` = conversations, events,
  sandbox lifecycle, the HTTP server) runs as **EH on the host** — a separate
  binary P's build does not touch. A change needing a new kernel capability must
  rebuild EH outside P. This is intentional: the agent evolves its policy (TS)
  and its executor-side Rust, **not** the trusted kernel.

---

## 7. Build phases
1. **`Policy` snapshot scope** (EH) + **`evolve_policy` tool** (deferred hand-off).
   Enough for TS-only evolution (path A) with snapshot/rewind.
2. **Policy supervisor** (outside P): bootstrap + evolve orchestration
   (build-in-P, restart EE, health-check, rewind-on-fail, report).
3. **Path-B image**: toolchain + `crates/` + warm cache, and run the guardian
   `build` inside P.

Phases 1–2 deliver the full self-evolution loop for TS. Phase 3 adds Rust
recompile, reusing the guardian's build inside P.

---

## 8. Build tooling: shared read-only toolchain volume (resolves the cache Q)

To keep the per-agent image small, share the heavy *immutable* build tooling and
keep only the per-agent *mutable* bits in the container:

| Thing | Where | Why |
| --- | --- | --- |
| Rust toolchain (rustc/cargo/std, ~2GB) | **shared RO volume** | generic, immutable, identical for every agent → never baked into the image |
| cargo registry (deps) | **shared RO volume** | pinned by `Cargo.lock`; same for all agents |
| `crates/` source (edited) | **P writable layer** | the agent edits it; must be snapshotted |
| `target/` (build output incl. binary) | **P writable layer** | snapshots must capture the binary so `rewind` rolls it back |

Driving constraint: snapshots must capture **source + binary** (so rollback
works), so those live in P's writable layer; the toolchain is generic/immutable,
so it's a shared RO mount and stays out of the image.

Mechanics:
- Provision once on the host: a volume with rustup + the pinned toolchain +
  `cargo fetch`ed deps (read-only).
- EH creates **P** with that volume mounted **read-only**, with `CARGO_HOME` /
  `RUSTUP_HOME` / `PATH` pointed at it. **Only the policy sandbox** mounts it —
  the work sandbox (`shell`) never builds exo. (Bonus: the extra mount also
  differs the policy spec from the work spec, which we already want.)
- Image stays light: node + tsx + exo binary + `crates/` source + manifests.

Caveats:
- **First build in a fresh P is a full compile** (slow, one-time); incremental
  afterward since `target/` persists in the writable layer (and in snapshots).
  Optional later: overlay a warm `target/` base RO under the writable layer.
- **RO registry ⇒ the agent cannot *add* new crate deps** (editing existing Rust
  is fine). Adding a dep needs a writable/re-provisioned registry.
- Isolation preserved: a read-only generic-toolchain mount exposes no kernel
  state or other agents' code (unlike the old repo-root rw mount).

## 9. Open questions
- **EE restart granularity** for a long-running REPL vs send/wakeup (the latter
  needs no explicit restart).
- **Promoting accepted changes into the image:** snapshots are the durable record;
  do we periodically re-bake the base image from the latest-good snapshot so a
  cold start doesn't replay a long snapshot chain?
- **Supervisor placement:** host process vs an EH-native `evolve_sandbox`
  operation (cleaner ownership, bigger kernel change).
