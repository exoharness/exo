import type {
  DurableFileSystem,
  Event,
  FileSystemMount,
  JsonValue,
  SandboxProcessLifecycle,
  SandboxProcessMode,
  SandboxProcessOutput,
  SandboxProcessStatus,
  SandboxProcessStdin,
  SandboxProvider,
} from "../api/protocol";
import { clampText, decodeBytes } from "./rendering";

export interface SandboxProcessView {
  id: string;
  name: string | null;
  command: string[];
  cwd: string | null;
  mode: SandboxProcessMode | null;
  stdin: SandboxProcessStdin | null;
  output: SandboxProcessOutput | null;
  lifecycle: SandboxProcessLifecycle | null;
  status: SandboxProcessStatus | null;
  providerState: JsonValue | null;
  stdoutCount: number;
  stderrCount: number;
  lastOutput: string | null;
}

export interface SandboxView {
  id: string;
  name: string | null;
  provider: SandboxProvider | null;
  image: string | null;
  defaultWorkdir: string | null;
  mounts: FileSystemMount[];
  durableFileSystems: DurableFileSystem[];
  enableNetworking: boolean | null;
  idleSeconds: number | null;
  state: "created" | "running" | "stopped" | "unknown";
  snapshots: string[];
  processes: SandboxProcessView[];
}

interface MutableSandboxView extends Omit<SandboxView, "processes"> {
  processes: Map<string, SandboxProcessView>;
}

export function deriveSandboxState(events: Event[]): SandboxView[] {
  const sandboxes = new Map<string, MutableSandboxView>();

  for (const event of events) {
    const data = event.data;
    if (!("sandbox_id" in data)) {
      continue;
    }

    const sandbox = ensureSandbox(sandboxes, data.sandbox_id);

    switch (data.type) {
      case "sandbox_created":
        sandbox.name = data.name ?? null;
        sandbox.provider = data.provider;
        sandbox.image = data.image;
        sandbox.defaultWorkdir = data.default_workdir;
        sandbox.mounts = data.file_system_mounts;
        sandbox.durableFileSystems = data.durable_file_systems ?? [];
        sandbox.enableNetworking = data.enable_networking;
        sandbox.idleSeconds = data.idle_seconds;
        sandbox.state = "created";
        break;
      case "sandbox_started":
        sandbox.state = "running";
        if (data.snapshot_id) {
          addUnique(sandbox.snapshots, data.snapshot_id);
        }
        break;
      case "sandbox_stopped":
        sandbox.state = "stopped";
        break;
      case "sandbox_snapshotted":
        addUnique(sandbox.snapshots, data.snapshot_id);
        break;
      case "sandbox_process_started": {
        const process = ensureProcess(sandbox, data.process_id);
        process.name = data.name ?? null;
        process.command = data.command;
        process.cwd = data.cwd;
        process.mode = data.mode;
        process.stdin = data.stdin;
        process.output = data.output;
        process.lifecycle = data.lifecycle;
        process.status = data.status;
        process.providerState = data.provider_state;
        break;
      }
      case "sandbox_process_state_updated": {
        const process = ensureProcess(sandbox, data.process_id);
        process.status = data.status;
        process.providerState = data.provider_state;
        break;
      }
      case "sandbox_process_event": {
        const process = ensureProcess(sandbox, data.process_id);
        if (data.event.type === "stdout") {
          process.stdoutCount += 1;
          process.lastOutput = clampText(decodeBytes(data.event.data), 400);
        } else if (data.event.type === "stderr") {
          process.stderrCount += 1;
          process.lastOutput = clampText(decodeBytes(data.event.data), 400);
        } else if (data.event.type === "exit") {
          process.status = { type: "exited", exit_code: data.event.exit_code };
        } else if (data.event.type === "error") {
          process.status = { type: "failed", message: data.event.message };
        } else if (data.event.type === "cancelled") {
          process.status = { type: "cancelled" };
        }
        break;
      }
    }
  }

  return Array.from(sandboxes.values()).map((sandbox) => ({
    ...sandbox,
    processes: Array.from(sandbox.processes.values()),
  }));
}

function ensureSandbox(
  map: Map<string, MutableSandboxView>,
  id: string,
): MutableSandboxView {
  const existing = map.get(id);
  if (existing) {
    return existing;
  }
  const sandbox: MutableSandboxView = {
    id,
    name: null,
    provider: null,
    image: null,
    defaultWorkdir: null,
    mounts: [],
    durableFileSystems: [],
    enableNetworking: null,
    idleSeconds: null,
    state: "unknown",
    snapshots: [],
    processes: new Map(),
  };
  map.set(id, sandbox);
  return sandbox;
}

function ensureProcess(
  sandbox: MutableSandboxView,
  id: string,
): SandboxProcessView {
  const existing = sandbox.processes.get(id);
  if (existing) {
    return existing;
  }
  const process: SandboxProcessView = {
    id,
    name: null,
    command: [],
    cwd: null,
    mode: null,
    stdin: null,
    output: null,
    lifecycle: null,
    status: null,
    providerState: null,
    stdoutCount: 0,
    stderrCount: 0,
    lastOutput: null,
  };
  sandbox.processes.set(id, process);
  return process;
}

function addUnique(values: string[], value: string): void {
  if (!values.includes(value)) {
    values.push(value);
  }
}
