import type { Env } from "./env";
export { ExoProvider } from "./provider";
export { ExoSandbox } from "./sandbox";
export { ExoEgress } from "./network";

declare global {
  namespace Cloudflare {
    interface GlobalProps {
      mainModule: typeof import("./index");
      durableNamespaces: "ExoProvider" | "ExoSandbox";
    }
  }
}

export default {
  async fetch(
    request: Request,
    env: Env,
    ctx: ExecutionContext,
  ): Promise<Response> {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/exo/"))
      return new Response("Not found", { status: 404 });
    const provider = env.PROVIDERS.getByName(env.ACCOUNT_ID);
    if (env.ACCESS_AUD !== undefined) {
      if (!env.ACCESS_AUD || ctx.access?.aud !== env.ACCESS_AUD)
        return new Response("Cloudflare Access required", { status: 403 });
      // Access context does not propagate to Durable Objects. Use the trusted
      // binding after verification, never a caller-supplied identity header.
      return provider.handleRequest(request);
    }
    return provider.fetch(request);
  },
} satisfies ExportedHandler<Env>;
