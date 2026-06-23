import { describe, expect, it } from "vitest";
import { deriveSandboxState } from "./sandbox";
import { makeEvent } from "../test/fixtures";

describe("deriveSandboxState", () => {
  it("returns empty array for no sandbox events", () => {
    expect(deriveSandboxState([makeEvent({ type: "turn_started" })])).toEqual(
      [],
    );
  });

  it("tracks lifecycle from created through running to stopped", () => {
    const events = [
      makeEvent({
        type: "sandbox_created",
        sandbox_id: "sb-1",
        name: "dev",
        provider: "local",
        image: "img:latest",
        default_workdir: "/work",
        file_system_mounts: [],
        enable_networking: true,
        idle_seconds: 60,
      }),
      makeEvent({
        type: "sandbox_started",
        sandbox_id: "sb-1",
        snapshot_id: "snap-a",
      }),
      makeEvent({
        type: "sandbox_stopped",
        sandbox_id: "sb-1",
      }),
    ];

    const [sandbox] = deriveSandboxState(events);
    expect(sandbox).toMatchObject({
      id: "sb-1",
      name: "dev",
      image: "img:latest",
      defaultWorkdir: "/work",
      enableNetworking: true,
      idleSeconds: 60,
      state: "stopped",
      snapshots: ["snap-a"],
    });
  });

  it("deduplicates snapshot ids across started and snapshotted events", () => {
    const events = [
      makeEvent({
        type: "sandbox_created",
        sandbox_id: "sb-1",
        provider: "local",
        image: "img",
        default_workdir: "/",
        file_system_mounts: [],
        enable_networking: false,
        idle_seconds: 0,
      }),
      makeEvent({
        type: "sandbox_started",
        sandbox_id: "sb-1",
        snapshot_id: "snap-1",
      }),
      makeEvent({
        type: "sandbox_snapshotted",
        sandbox_id: "sb-1",
        snapshot_id: "snap-1",
      }),
      makeEvent({
        type: "sandbox_snapshotted",
        sandbox_id: "sb-1",
        snapshot_id: "snap-2",
      }),
    ];

    const [sandbox] = deriveSandboxState(events);
    expect(sandbox.snapshots).toEqual(["snap-1", "snap-2"]);
  });

  it("accumulates process output events and updates status on exit", () => {
    const helloBytes = [...new TextEncoder().encode("hello")];
    const errBytes = [...new TextEncoder().encode("boom")];

    const events = [
      makeEvent({
        type: "sandbox_process_started",
        sandbox_id: "sb-1",
        process_id: "proc-1",
        name: "shell",
        command: ["sh", "-c", "echo hi"],
        cwd: "/work",
        mode: "exec",
        stdin: "none",
        output: "stream",
        lifecycle: "detached",
        status: { type: "running" },
        provider_state: null,
      }),
      makeEvent({
        type: "sandbox_process_event",
        sandbox_id: "sb-1",
        process_id: "proc-1",
        event: { type: "stdout", cursor: 0, data: helloBytes },
      }),
      makeEvent({
        type: "sandbox_process_event",
        sandbox_id: "sb-1",
        process_id: "proc-1",
        event: { type: "stderr", cursor: 1, data: errBytes },
      }),
      makeEvent({
        type: "sandbox_process_state_updated",
        sandbox_id: "sb-1",
        process_id: "proc-1",
        status: { type: "running" },
        provider_state: { phase: "busy" },
      }),
      makeEvent({
        type: "sandbox_process_event",
        sandbox_id: "sb-1",
        process_id: "proc-1",
        event: { type: "exit", cursor: 2, exit_code: 0 },
      }),
    ];

    const sandboxes = deriveSandboxState(events);
    const process = sandboxes[0]?.processes[0];
    expect(process).toMatchObject({
      id: "proc-1",
      name: "shell",
      stdoutCount: 1,
      stderrCount: 1,
      lastOutput: "boom",
      status: { type: "exited", exit_code: 0 },
      providerState: { phase: "busy" },
    });
  });

  it("maps process error and cancelled events to failed/cancelled status", () => {
    const events = [
      makeEvent({
        type: "sandbox_process_event",
        sandbox_id: "sb-1",
        process_id: "proc-err",
        event: { type: "error", cursor: 0, message: "disk full" },
      }),
      makeEvent({
        type: "sandbox_process_event",
        sandbox_id: "sb-1",
        process_id: "proc-cancel",
        event: { type: "cancelled", cursor: 0 },
      }),
    ];

    const sandboxes = deriveSandboxState(events);
    const byId = Object.fromEntries(
      sandboxes[0]!.processes.map((process) => [process.id, process]),
    );
    expect(byId["proc-err"]?.status).toEqual({
      type: "failed",
      message: "disk full",
    });
    expect(byId["proc-cancel"]?.status).toEqual({ type: "cancelled" });
  });
});
