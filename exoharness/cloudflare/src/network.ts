import { WorkerEntrypoint } from "cloudflare:workers";
import type { Env, SandboxIdentity } from "./env";

export interface ProxyHeaders {
  headers: [string, string][];
  credentials: [string, string][];
  refreshed: boolean;
}

export type ProxyReply = ProxyHeaders | { status: number; error: string };

export interface ProxyRequest {
  url: string;
  method: string;
  headers: [string, string][];
}

export async function proxyEgress(
  identity: SandboxIdentity,
  request: Request,
  provider:
    | Pick<import("./provider").ExoProvider, "proxyHeaders">
    | Pick<DurableObjectStub<import("./provider").ExoProvider>, "proxyHeaders">,
): Promise<Response> {
  const input: ProxyRequest = {
    url: request.url,
    method: request.method,
    headers: [...request.headers],
  };
  let policy: ProxyReply;
  try {
    policy = (await provider.proxyHeaders(identity, input)) as ProxyReply;
  } catch (error) {
    console.error("Exo egress policy lookup failed", String(error));
    return new Response("Exo egress policy lookup failed", { status: 502 });
  }
  if ("error" in policy) {
    console.warn("Exo egress policy rejected request", policy.error);
    return new Response("Exo egress policy rejected request", {
      status: policy.status,
    });
  }
  // A tee buffers unread uploads while the first request runs. Only retry
  // bodyless requests; streaming uploads go upstream once without buffering.
  const retry = policy.credentials.length > 0 && request.body === null;
  const send = (headers: [string, string][]) =>
    fetch(new Request(request, { headers, redirect: "manual" }));
  try {
    let response = await send(policy.headers);
    if (response.status === 401 && retry) {
      const fresh = (await provider.proxyHeaders(
        identity,
        input,
        policy.credentials,
      )) as ProxyReply;
      if ("error" in fresh) {
        await response.body?.cancel();
        console.error("Exo egress credential retry failed", fresh.error);
        return new Response("Exo egress credential retry failed", {
          status: fresh.status,
        });
      }
      if (fresh.refreshed) {
        await response.body?.cancel();
        response = await send(fresh.headers);
      }
    }
    return response;
  } catch (error) {
    console.error("Exo egress request failed", String(error));
    return new Response("Exo egress request failed", { status: 502 });
  }
}

// Native interception supplies trusted identity props, independent of sandbox headers.
export class ExoEgress extends WorkerEntrypoint<Env, SandboxIdentity> {
  fetch(request: Request): Promise<Response> {
    return proxyEgress(
      this.ctx.props,
      request,
      this.env.PROVIDERS.getByName(this.env.ACCOUNT_ID),
    );
  }
}
