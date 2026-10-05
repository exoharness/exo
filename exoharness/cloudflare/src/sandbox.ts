import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";
import type { SandboxProcessStartRequest } from "../../typescript/harness/index";
import type { Env, ExecRequest, ExecResult, SandboxIdentity } from "./env";

const ca = "/etc/cloudflare/certs/cloudflare-containers-ca.crt";
const idleTimeoutMs = 300_000;
const managedImage = "cloudflare/debian-trixie";

export class CloudflareProcess extends RpcTarget {
  private readonly stdoutPipe: ReadableStream<Uint8Array>;
  private readonly stderrPipe: ReadableStream<Uint8Array>;
  private readonly writer: WritableStreamDefaultWriter<Uint8Array>;
  private inputClosed = false;
  private exited = false;
  private closing?: Promise<void>;

  constructor(private readonly process: ExecProcess) {
    super();
    if (!process.stdin || !process.stdout || !process.stderr)
      throw new Error("sandbox process pipes are unavailable");
    this.writer = process.stdin.getWriter();
    this.stdoutPipe = process.stdout;
    this.stderrPipe = process.stderr;
  }
  get stdout(): ReadableStream<Uint8Array> {
    return this.stdoutPipe;
  }
  get stderr(): ReadableStream<Uint8Array> {
    return this.stderrPipe;
  }
  get sandboxProcessId(): string {
    return String(this.process.pid);
  }
  async writeStdin(data: Uint8Array): Promise<void> {
    await this.writer.write(data);
  }
  async closeStdin(): Promise<void> {
    if (!this.inputClosed) {
      this.inputClosed = true;
      await this.writer.close();
    }
  }
  async close(): Promise<void> {
    if (this.exited) return;
    this.closing ??= this.terminate();
    await this.closing;
  }
  private async terminate(): Promise<void> {
    try {
      await this.closeStdin();
    } finally {
      if (!this.exited) {
        this.process.kill(15);
        const timer = setTimeout(() => {
          if (!this.exited) this.process.kill(9);
        }, 5000);
        try {
          await this.process.exitCode;
        } finally {
          clearTimeout(timer);
        }
      }
    }
  }
  async wait(): Promise<number> {
    try {
      return await this.process.exitCode;
    } finally {
      this.exited = true;
    }
  }
}

// Caller-side adapter: Cloudflare RPC capabilities, invocation lifetimes, and
// byte streams are hidden behind the ordinary SandboxProcess interface.
export class CloudflareSandbox {
  constructor(
    private readonly stub: DurableObjectStub<ExoSandbox>,
    private readonly identity: SandboxIdentity,
    private readonly waitUntil: (promise: Promise<unknown>) => void,
  ) {}

  async openProcess(request: SandboxProcessStartRequest & { cwd?: string }) {
    const ready = new Promise<RpcStub<CloudflareProcess>>((resolve, reject) => {
      const running = this.stub.runProcess(
        this.identity,
        request,
        async (process) => {
          resolve(process.dup());
        },
      );
      this.waitUntil(running.catch(reject));
    });
    const process = await ready;
    try {
      const [stdout, stderr, sandboxProcessId] = await Promise.all([
        process.stdout,
        process.stderr,
        process.sandboxProcessId,
      ]);
      let waiting: Promise<number> | undefined;
      let disposed = false;
      const dispose = () => {
        if (!disposed) {
          disposed = true;
          process[Symbol.dispose]();
        }
      };
      return {
        sandboxId: this.identity.sandboxId,
        sandboxProcessId,
        reused: false,
        stdout,
        stderr,
        writeStdin: (data: Uint8Array) => process.writeStdin(data),
        closeStdin: () => process.closeStdin(),
        close: async () => {
          try {
            if (!disposed) await process.close();
          } finally {
            dispose();
          }
        },
        wait: () => {
          waiting ??= process.wait().finally(dispose);
          return waiting;
        },
      };
    } catch (error) {
      try {
        await process.close();
      } finally {
        process[Symbol.dispose]();
      }
      throw error;
    }
  }
}

export class ExoSandbox extends DurableObject<Env> {
  private starting?: Promise<Container>;
  private stopping?: Promise<void>;
  // Leases live only in this Durable Object instance. Eviction loses them;
  // the shared runtime's recovery scan handles an interrupted turn.
  private readonly activities = new Set<string>();
  private activeExecs = 0;
  private readonly processes = new Set<CloudflareProcess>();
  private idleMs = idleTimeoutMs;

  async acquire(
    identity: SandboxIdentity,
    image: string,
    cwd: string,
    environment: Record<string, string>,
    snapshot: { id: string } | null,
    idleMs: number,
  ): Promise<string> {
    const container = this.ctx.container;
    if (!container) throw new Error("native sandbox binding missing");
    const name = image || managedImage;
    const reference = name === managedImage ? name : container.images[name];
    if (!reference)
      throw new Error(`Cloudflare sandbox image is not configured: ${name}`);
    this.activeExecs++;
    try {
      await this.stopping;
      if (snapshot) {
        await this.stop();
        await this.ctx.storage.put("snapshot", snapshot);
      }
      this.idleMs = idleMs;
      await this.ctx.storage.put("idleMs", idleMs);
      await this.ctx.storage.put("cwd", cwd);
      await this.ctx.storage.put("environment", environment);
      await this.ctx.storage.put("image", reference);
      await this.readyContainer(identity);
      return name;
    } finally {
      this.activeExecs--;
      await this.scheduleIdle();
    }
  }

  async info(
    identity: SandboxIdentity,
  ): Promise<{ exists: boolean; running: boolean }> {
    const saved = await this.ctx.storage.get<SandboxIdentity>("identity");
    if (
      saved &&
      (saved.agentId !== identity.agentId ||
        saved.threadId !== identity.threadId ||
        saved.sandboxId !== identity.sandboxId)
    )
      throw new Error("sandbox identity mismatch");
    return { exists: !!saved, running: this.ctx.container?.running ?? false };
  }

  async terminate(identity: SandboxIdentity): Promise<void> {
    await this.info(identity);
    await this.stop();
    await this.ctx.storage.deleteAll();
  }

  async beginActivity(identity: SandboxIdentity, id: string): Promise<void> {
    this.activities.add(id);
    // A checkpoint may take longer than blockConcurrencyWhile's time limit.
    // New work waits for that checkpoint before restarting the sandbox.
    await (await this.readyContainer(identity)).setInactivityTimeout(660_000);
  }

  async endActivity(id: string): Promise<void> {
    if (this.activities.delete(id) && this.activities.size === 0)
      await this.scheduleIdle();
  }

  private async scheduleIdle(): Promise<void> {
    if (!this.ctx.container?.running) return;
    await this.ctx.storage.setAlarm(Date.now() + this.idleMs);
    // The alarm checkpoints before the platform's inactivity shutdown.
    // Active turns postpone the alarm; the Rust runtime owns cancellation.
    await this.ctx.container.setInactivityTimeout(this.idleMs + 60_000);
  }

  async alarm(): Promise<void> {
    const scheduled = await this.ctx.storage.getAlarm();
    if (
      this.activities.size > 0 ||
      this.activeExecs > 0 ||
      (scheduled !== null && scheduled > Date.now())
    )
      return;
    await this.stop();
  }

  private async start(identity: SandboxIdentity): Promise<Container> {
    this.idleMs =
      (await this.ctx.storage.get<number>("idleMs")) ?? idleTimeoutMs;
    const saved = await this.ctx.storage.get<SandboxIdentity>("identity");
    if (
      saved &&
      (saved.threadId !== identity.threadId ||
        saved.agentId !== identity.agentId ||
        saved.sandboxId !== identity.sandboxId)
    )
      throw new Error("sandbox identity mismatch");
    await this.ctx.storage.put("identity", identity);
    const container = this.ctx.container;
    if (!container) throw new Error("native sandbox binding missing");
    if (!container.running) {
      const egress = this.ctx.exports.ExoEgress({ props: identity });
      await container.interceptAllOutboundHttp(egress);
      await container.interceptOutboundHttps("*", egress);
      const snapshot =
        await this.ctx.storage.get<ContainerSnapshot>("snapshot");
      const image = await this.ctx.storage.get<string>("image");
      if (!image) throw new Error("sandbox has not been acquired");
      const options = {
        entrypoint: ["sleep", "infinity"],
        enableInternet: false,
        instance: "lite" as const,
      };
      container.start(
        snapshot
          ? { ...options, containerSnapshot: { id: snapshot.id } }
          : { ...options, image },
      );
      const cwd = await this.ctx.storage.get<string>("cwd");
      const setup = await container.exec(["mkdir", "-p", cwd ?? "/workspace"]);
      const setupOutput = await setup.output();
      if (setupOutput.exitCode !== 0)
        throw new Error(
          `sandbox setup failed (${setupOutput.exitCode}): ${new TextDecoder().decode(setupOutput.stderr)}`,
        );
    }
    await container.setInactivityTimeout(
      this.activities.size === 0 ? this.idleMs + 60_000 : 660_000,
    );
    return container;
  }

  private async readyContainer(identity: SandboxIdentity): Promise<Container> {
    await this.stopping;
    await this.ctx.storage.deleteAlarm();
    this.starting ??= this.start(identity).finally(() => {
      this.starting = undefined;
    });
    return this.starting;
  }

  async exec(
    identity: SandboxIdentity,
    request: ExecRequest,
  ): Promise<ExecResult> {
    this.activeExecs++;
    try {
      return await this.execCommand(identity, request);
    } finally {
      this.activeExecs--;
      if (this.activities.size === 0 && this.activeExecs === 0)
        await this.scheduleIdle();
    }
  }

  private async execCommand(
    identity: SandboxIdentity,
    request: ExecRequest,
  ): Promise<ExecResult> {
    const container = await this.readyContainer(identity);
    const options = await this.execOptions(request);
    const process = await container.exec(request.command, {
      ...options,
    });
    const timer =
      request.timeoutMs === undefined
        ? undefined
        : setTimeout(() => process.kill(9), request.timeoutMs);
    try {
      const output = await process.output();
      const decoder = new TextDecoder();
      return {
        stdout: decoder.decode(output.stdout),
        stderr: decoder.decode(output.stderr),
        exitCode: output.exitCode,
      };
    } finally {
      if (timer !== undefined) clearTimeout(timer);
    }
  }

  private async startProcess(
    identity: SandboxIdentity,
    request: SandboxProcessStartRequest & { cwd?: string },
  ): Promise<CloudflareProcess> {
    const container = await this.readyContainer(identity);
    const process = await container.exec(request.command, {
      ...(await this.execOptions(request)),
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    return new CloudflareProcess(process);
  }

  private async execOptions(request: Pick<ExecRequest, "env" | "cwd">) {
    return {
      cwd:
        request.cwd ??
        (await this.ctx.storage.get<string>("cwd")) ??
        "/workspace",
      env: {
        HOME: "/home/exo",
        ...request.env,
        ...(await this.ctx.storage.get<Record<string, string>>("environment")),
        NODE_EXTRA_CA_CERTS: ca,
        REQUESTS_CA_BUNDLE: ca,
        SSL_CERT_FILE: ca,
        CURL_CA_BUNDLE: ca,
        GIT_SSL_CAINFO: ca,
      },
    };
  }

  async runProcess(
    identity: SandboxIdentity,
    request: SandboxProcessStartRequest & { cwd?: string },
    ready: (process: RpcStub<CloudflareProcess>) => Promise<void>,
  ): Promise<void> {
    // Native exec handles belong to the invocation that created them. Keep it
    // alive while the caller consumes the pipes and controls the RPC target.
    const process = await this.startProcess(identity, request);
    this.processes.add(process);
    try {
      const capability = new RpcStub(process);
      try {
        await ready(capability);
        await process.wait();
      } finally {
        capability[Symbol.dispose]();
      }
    } finally {
      this.processes.delete(process);
    }
  }

  async waitForProcesses(): Promise<void> {
    await Promise.all([...this.processes].map((process) => process.wait()));
  }

  async snapshot(identity: SandboxIdentity): Promise<ContainerSnapshot> {
    this.activeExecs++;
    try {
      const container = await this.readyContainer(identity);
      const snapshot = await container.snapshotContainer();
      await this.ctx.storage.put("snapshot", snapshot);
      return snapshot;
    } finally {
      this.activeExecs--;
      if (this.activities.size === 0 && this.activeExecs === 0)
        await this.scheduleIdle();
    }
  }

  async stop(): Promise<void> {
    this.activities.clear();
    this.stopping ??= this.checkpointAndStop().finally(() => {
      this.stopping = undefined;
    });
    await this.stopping;
  }

  private async checkpointAndStop(): Promise<void> {
    await this.ctx.storage.deleteAlarm();
    const container = this.ctx.container;
    if (!container?.running) return;
    // End process sessions before checkpointing so applications can flush
    // their state. Turns do not wait for either operation.
    await Promise.all([...this.processes].map((process) => process.close()));
    const snapshot = await container.snapshotContainer();
    await this.ctx.storage.put("snapshot", snapshot);
    await container.destroy();
  }
}
