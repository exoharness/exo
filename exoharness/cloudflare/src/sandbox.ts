import { DurableObject } from "cloudflare:workers";
import type { Env, ExecRequest, ExecResult, SandboxIdentity } from "./env";

const ca = "/etc/cloudflare/certs/cloudflare-containers-ca.crt";

export class ExoSandbox extends DurableObject<Env> {
  private starting?: Promise<Container>;

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
