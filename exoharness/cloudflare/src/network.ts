import { WorkerEntrypoint } from "cloudflare:workers";
import type { Env, SandboxIdentity, SandboxPolicy } from "./env";

export const PLACEHOLDER_PREFIX = "exo_egress_";
const routingHeaders = new Set([
  "host",
  "content-length",
  "connection",
  "transfer-encoding",
  "upgrade",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
]);

export function validateOrigin(value: string): string {
  const url = new URL(value);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    value !== url.origin ||
    url.username ||
    url.password
  )
    throw new Error("network rules must be exact HTTP(S) origins");
  if (
    url.hostname === "localhost" ||
    url.hostname.endsWith(".localhost") ||
    url.hostname.endsWith(".internal") ||
    url.hostname.endsWith(".local") ||
    /^[\d.]+$/.test(url.hostname) ||
    url.hostname.startsWith("[")
  )
    throw new Error("network rules require public DNS names");
  if (url.port)
    throw new Error("the prototype intercepts only ports 80 and 443");
  return url.origin;
}

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

export async function proxyRequest(
  request: Request,
  policy: SandboxPolicy,
  resolve: (credential: string, url: string) => Promise<string>,
  upstream: typeof fetch = fetch,
): Promise<Response> {
  const url = new URL(request.url);
  if (
    url.username ||
    url.password ||
    url.hash ||
    !policy.origins.includes(url.origin)
  )
    throw new Error("network destination denied");
  const headers = new Headers(request.headers);
  const connectionHeaders = new Set(
    (headers.get("connection") ?? "")
      .split(",")
      .map((part) => part.trim().toLowerCase()),
  );
  for (const [name, header] of headers) {
    let value = header;
    let basic = false;
    if (name === "authorization" && /^Basic /i.test(value)) {
      value = atob(value.slice(6));
      basic = true;
    }
    if (!value.includes(PLACEHOLDER_PREFIX)) continue;
    if (
      url.protocol !== "https:" ||
      routingHeaders.has(name) ||
      connectionHeaders.has(name)
    )
      throw new Error("credential cannot rewrite routing or use HTTP");
    // Resolve everything before dispatch; a partially replaced request is never sent.
    for (const binding of policy.credentials) {
      if (value.includes(binding.placeholder))
        value = value.replaceAll(
          binding.placeholder,
          await resolve(binding.credential, url.href),
        );
    }
    if (
      value.includes(PLACEHOLDER_PREFIX) ||
      ["\r", "\n", "\0"].some((character) => value.includes(character))
    )
      throw new Error("unknown or invalid credential placeholder");
    headers.set(name, basic ? `Basic ${btoa(value)}` : value);
  }
  for (const name of [...routingHeaders, ...connectionHeaders])
    headers.delete(name);
  // Do not follow redirects with resolved credentials. The sandbox may follow
  // the response, and its next request passes the destination checks again.
  return upstream(
    new Request(url.href, {
      method: request.method,
      headers,
      body: request.body,
      redirect: "manual",
    }),
  );
}
