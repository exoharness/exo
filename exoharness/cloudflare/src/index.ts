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
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    // Optional live-test target, using a synthetic secret rather than a real API.
    if (url.pathname === "/_probe" && env.PROBE_KEY)
      return Response.json({
        authenticated:
          request.headers.get("authorization") === `Bearer ${env.PROBE_KEY}` ||
          request.headers.get("authorization") ===
            `Basic ${btoa(`user:${env.PROBE_KEY}`)}`,
        placeholderReceived: (
          request.headers.get("authorization") ?? ""
        ).includes("exo_egress_"),
      });
    if (
      !env.EXO_TOKEN ||
      request.headers.get("authorization") !== `Bearer ${env.EXO_TOKEN}`
    )
      return new Response("Unauthorized", { status: 401 });
    if (!url.pathname.startsWith("/exo/"))
      return new Response("Not found", { status: 404 });
    return env.PROVIDERS.getByName(env.ACCOUNT_ID).fetch(request);
  },
} satisfies ExportedHandler<Env>;
