import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";
import type {
  SandboxProcess,
  SandboxProcessStartRequest,
} from "../../typescript/harness/index";
import codexPackage from "./codex-package.json";
import type { Env, ExecRequest, ExecResult, SandboxIdentity } from "./env";

const ca = "/etc/cloudflare/certs/cloudflare-containers-ca.crt";
const idleTimeoutMs = 300_000;

// The Worker downloads a pinned package over its trusted connection and pipes it
// into the sandbox. Agent egress never gains access to the package registry.
const installCodex = `
const fs = require('node:fs');
const {createHash} = require('node:crypto');
const {Transform} = require('node:stream');
const {pipeline} = require('node:stream/promises');
const {execFileSync} = require('node:child_process');
(async () => {
  const root = process.argv[1], integrity = process.argv[2];
  const hash = createHash('sha512');
  await pipeline(process.stdin, new Transform({transform(chunk, _, cb) {
    hash.update(chunk); cb(null, chunk);
  }}), fs.createWriteStream(root + '.tgz'));
  if ('sha512-' + hash.digest('base64') !== integrity) throw Error('Codex package integrity mismatch');
  fs.mkdirSync(root, {recursive:true});
  execFileSync('tar', ['-xzf', root + '.tgz', '-C', root, '--strip-components=1']);
  const vendor = root + '/vendor/x86_64-unknown-linux-musl';
  for (const [name, path] of [['codex', '/bin/codex'], ['codex-code-mode-host', '/bin/codex-code-mode-host'], ['rg', '/codex-path/rg']]) {
    const link = '/usr/local/bin/' + name;
    if (fs.existsSync(link)) fs.unlinkSync(link);
    fs.symlinkSync(vendor + path, link);
  }
  fs.unlinkSync(root + '.tgz');
})().catch(e => { console.error(e.message); process.exit(1); });`;

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

  async runTurn<T>(turnId: string, run: () => Promise<T>): Promise<T> {
    await this.stub.beginTurn(this.identity, turnId);
    try {
      return await run();
    } finally {
      await this.stub.endTurn(turnId);
      // Pending operations keep a Durable Object alive for at most 15 minutes
      // from their start. Refresh both sides' keepalive after each turn so a
      // long conversation can still use the full idle window.
      this.waitUntil(this.stub.waitForProcesses());
    }
  }

  async prepareCodex(version: string): Promise<void> {
    await this.stub.prepareCodex(this.identity, version);
  }

  async startProcess(
    request: SandboxProcessStartRequest,
  ): Promise<SandboxProcess> {
    const process = await this.openProcess(request);
    return {
      ...process,
      stdout: process.stdout.pipeThrough(new TextDecoderStream()),
      stderr: process.stderr.pipeThrough(new TextDecoderStream()),
      writeStdin: (data) => process.writeStdin(new TextEncoder().encode(data)),
    };
  }

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
      let closing: Promise<void> | undefined;
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
        close: () => {
          closing ??= (async () => {
            try {
              if (!disposed) await process.close();
            } finally {
              dispose();
            }
          })();
          return closing;
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
  private installing?: Promise<void>;
  private stopping?: Promise<void>;
  private activeTurn: string | null = null;
  private activeExecs = 0;
  private readonly processes = new Set<CloudflareProcess>();
  private idleMs = idleTimeoutMs;

  async acquire(
    identity: SandboxIdentity,
    cwd: string,
    environment: Record<string, string>,
    snapshot: { id: string } | null,
    idleMs: number,
  ): Promise<void> {
    await this.stopping;
    if (snapshot) {
      await this.stop();
      await this.ctx.storage.put("snapshot", snapshot);
    }
    this.idleMs = idleMs;
    await this.ctx.storage.put("idleMs", idleMs);
    await this.ctx.storage.put("cwd", cwd);
    await this.ctx.storage.put("environment", environment);
    await this.start(identity);
    await this.scheduleIdle();
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

  async beginTurn(identity: SandboxIdentity, turnId: string): Promise<void> {
    this.activeTurn = turnId;
    // A checkpoint may take longer than blockConcurrencyWhile's time limit.
    // New work waits for that checkpoint before restarting the sandbox.
    await this.stopping;
    await this.ctx.storage.deleteAlarm();
    await this.start(identity);
  }

  async endTurn(turnId: string): Promise<void> {
    if (this.activeTurn !== turnId) return;
    this.activeTurn = null;
    await this.scheduleIdle();
  }

  private async scheduleIdle(): Promise<void> {
    if (!this.ctx.container?.running) return;
    await this.ctx.storage.setAlarm(Date.now() + this.idleMs);
    // The alarm checkpoints before the platform's inactivity shutdown. During
    // a turn, the runtime's ten-minute limit owns cancellation instead.
    await this.ctx.container.setInactivityTimeout(this.idleMs + 60_000);
  }

  async alarm(): Promise<void> {
    const scheduled = await this.ctx.storage.getAlarm();
    if (
      this.activeTurn !== null ||
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
      const options = {
        entrypoint: ["sleep", "infinity"],
        enableInternet: false,
        instance: "lite" as const,
      };
      container.start(
        snapshot
          ? { ...options, containerSnapshot: { id: snapshot.id } }
          : { ...options, image: "cloudflare/debian-trixie" },
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
      this.activeTurn === null ? this.idleMs + 60_000 : 660_000,
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
      if (this.activeTurn === null && this.activeExecs === 0)
        await this.scheduleIdle();
    }
  }

  private async execCommand(
    identity: SandboxIdentity,
    request: ExecRequest,
  ): Promise<ExecResult> {
    if (
      !Array.isArray(request.command) ||
      !request.command.length ||
      request.command.some(
        (part) => typeof part !== "string" || part.includes("\0"),
      )
    )
      throw new Error("command must be an argv array");
    const timeoutMs = request.timeoutMs ?? 60_000;
    if (!Number.isInteger(timeoutMs) || timeoutMs < 1 || timeoutMs > 120_000)
      throw new Error("timeout must be between 1 and 120000 ms");
    const container = await this.readyContainer(identity);
    const options = await this.execOptions(request);
    const process = await container.exec(request.command, {
      ...options,
    });
    // Abort destroys the whole instance, including descendants, before returning
    // an ambiguous result. We never retry the command automatically.
    const timer = setTimeout(() => {
      this.ctx.waitUntil(container.destroy());
    }, timeoutMs);
    try {
      const output = await process.output();
      const decoder = new TextDecoder();
      return {
        stdout: decoder.decode(output.stdout),
        stderr: decoder.decode(output.stderr),
        exitCode: output.exitCode,
      };
    } finally {
      clearTimeout(timer);
    }
  }

  async prepareCodex(
    identity: SandboxIdentity,
    version: string,
  ): Promise<void> {
    if (version !== codexPackage.version)
      throw new Error(
        "Codex package pin differs from the native harness version",
      );
    const container = await this.readyContainer(identity);
    this.installing ??= this.installCodex(container).finally(() => {
      this.installing = undefined;
    });
    await this.installing;
  }

  private async installCodex(container: Container): Promise<void> {
    const check = await container.exec([
      "sh",
      "-c",
      "if test -x /usr/local/bin/codex; then /usr/local/bin/codex --version; fi",
    ]);
    const output = await check.output();
    if (
      output.exitCode === 0 &&
      new TextDecoder().decode(output.stdout).trim() ===
        `codex-cli ${codexPackage.version}`
    )
      return;
    const response = await fetch(codexPackage.url, {
      signal: AbortSignal.timeout(120_000),
    });
    if (!response.ok || !response.body)
      throw new Error(`Codex package download failed (${response.status})`);
    const setup = await container.exec(
      [
        "node",
        "-e",
        installCodex,
        `/opt/exo-codex-${codexPackage.version}`,
        codexPackage.integrity,
      ],
      { stdin: response.body },
    );
    const installed = await setup.output();
    if (installed.exitCode !== 0)
      throw new Error(
        `Codex installation failed: ${new TextDecoder().decode(installed.stderr)}`,
      );
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
        CODEX_HOME: "/home/exo/.codex",
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
      await this.stopping;
      await this.ctx.storage.deleteAlarm();
      const container = await this.start(identity);
      const snapshot = await container.snapshotContainer();
      await this.ctx.storage.put("snapshot", snapshot);
      return snapshot;
    } finally {
      this.activeExecs--;
      if (this.activeTurn === null && this.activeExecs === 0)
        await this.scheduleIdle();
    }
  }

  async stop(): Promise<void> {
    this.activeTurn = null;
    this.stopping ??= this.checkpointAndStop().finally(() => {
      this.stopping = undefined;
    });
    await this.stopping;
  }

  private async checkpointAndStop(): Promise<void> {
    await this.ctx.storage.deleteAlarm();
    const container = this.ctx.container;
    if (!container?.running) return;
    // End the process session before saving its filesystem, so Codex's
    // history is flushed. Turns do not wait for either operation.
    await Promise.all([...this.processes].map((process) => process.close()));
    const snapshot = await container.snapshotContainer();
    await this.ctx.storage.put("snapshot", snapshot);
    await container.destroy();
  }
}
