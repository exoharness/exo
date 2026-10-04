import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";
import type { SandboxProcessStartRequest } from "../../typescript/harness/core";
import codexPackage from "./codex-package.json";
import type { Env, ExecRequest, ExecResult, SandboxIdentity } from "./env";

const ca = "/etc/cloudflare/certs/cloudflare-containers-ca.crt";

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

  constructor(private readonly process: ExecProcess) {
    super();
    if (!process.stdin || !process.stdout || !process.stderr)
      throw new Error("Codex process pipes are unavailable");
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
  async writeStdin(data: string): Promise<void> {
    await this.writer.write(new TextEncoder().encode(data));
  }
  async closeStdin(): Promise<void> {
    if (!this.inputClosed) {
      this.inputClosed = true;
      await this.writer.close();
    }
  }
  async close(): Promise<void> {
    await this.closeStdin();
    const timer = setTimeout(() => this.process.kill(9), 5000);
    try {
      await this.process.exitCode;
    } finally {
      clearTimeout(timer);
    }
  }
  async wait(): Promise<number> {
    return this.process.exitCode;
  }
}

export class ExoSandbox extends DurableObject<Env> {
  private starting?: Promise<Container>;
  private installing?: Promise<void>;

  private async start(identity: SandboxIdentity): Promise<Container> {
    const saved = await this.ctx.storage.get<SandboxIdentity>("identity");
    if (
      saved &&
      (saved.threadId !== identity.threadId ||
        saved.agentId !== identity.agentId)
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
      const setup = await container.exec(["mkdir", "-p", "/workspace"]);
      const setupOutput = await setup.output();
      if (setupOutput.exitCode !== 0)
        throw new Error(
          `sandbox setup failed (${setupOutput.exitCode}): ${new TextDecoder().decode(setupOutput.stderr)}`,
        );
    }
    await container.setInactivityTimeout(300_000);
    return container;
  }

  async exec(
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
    this.starting ??= this.start(identity).finally(() => {
      this.starting = undefined;
    });
    const container = await this.starting;
    const policy = await this.env.PROVIDERS.getByName(
      this.env.ACCOUNT_ID,
    ).sandboxPolicy(identity);
    const environment = Object.fromEntries(
      policy.credentials.map((binding) => [
        binding.environmentVariable,
        binding.placeholder,
      ]),
    );
    const process = await container.exec(request.command, {
      cwd: "/workspace",
      env: {
        ...request.env,
        ...environment,
        NODE_EXTRA_CA_CERTS: ca,
        REQUESTS_CA_BUNDLE: ca,
        SSL_CERT_FILE: ca,
        CURL_CA_BUNDLE: ca,
        GIT_SSL_CAINFO: ca,
      },
      signal: AbortSignal.timeout(timeoutMs),
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

  private async prepareCodex(container: Container): Promise<void> {
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

  async startCodexProcess(
    identity: SandboxIdentity,
    request: SandboxProcessStartRequest,
  ): Promise<CloudflareProcess> {
    this.starting ??= this.start(identity).finally(() => {
      this.starting = undefined;
    });
    const container = await this.starting;
    this.installing ??= this.prepareCodex(container).finally(() => {
      this.installing = undefined;
    });
    await this.installing;
    const policy = await this.env.PROVIDERS.getByName(
      this.env.ACCOUNT_ID,
    ).sandboxPolicy(identity);
    const environment = Object.fromEntries(
      policy.credentials.map((binding) => [
        binding.environmentVariable,
        binding.placeholder,
      ]),
    );
    const process = await container.exec(request.command, {
      cwd: "/workspace",
      env: {
        ...request.env,
        ...environment,
        HOME: "/home/exo",
        CODEX_HOME: "/home/exo/.codex",
        NODE_EXTRA_CA_CERTS: ca,
        SSL_CERT_FILE: ca,
        CURL_CA_BUNDLE: ca,
        GIT_SSL_CAINFO: ca,
      },
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    return new CloudflareProcess(process);
  }

  async runCodexProcess(
    identity: SandboxIdentity,
    request: SandboxProcessStartRequest,
    ready: (process: RpcStub<CloudflareProcess>) => Promise<void>,
  ): Promise<void> {
    // Native exec handles belong to the invocation that created them. Keep it
    // alive while the caller consumes the pipes and controls the RPC target.
    const process = await this.startCodexProcess(identity, request);
    await ready(new RpcStub(process));
    await process.wait();
  }

  async snapshot(identity: SandboxIdentity): Promise<ContainerSnapshot> {
    const container = await this.start(identity);
    const snapshot = await container.snapshotContainer();
    await this.ctx.storage.put("snapshot", snapshot);
    return snapshot;
  }

  async stop(): Promise<void> {
    if (this.ctx.container) await this.ctx.container.destroy();
  }
}
