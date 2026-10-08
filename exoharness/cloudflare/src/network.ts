import { WorkerEntrypoint } from "cloudflare:workers";
import type { Env, SandboxIdentity } from "./env";

// Native sandbox interception supplies this binding's props. A command cannot
// impersonate a different thread using headers, URL parameters or an env var.
export class ExoEgress extends WorkerEntrypoint<Env, SandboxIdentity> {
  async fetch(request: Request): Promise<Response> {
    try {
      const provider = this.env.PROVIDERS.getByName(this.env.ACCOUNT_ID);
      return await provider.proxy(this.ctx.props, request);
    } catch {
      return new Response("Exo egress denied", { status: 403 });
    }
  }
}
