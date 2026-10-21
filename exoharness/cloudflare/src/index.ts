import type { Env } from "./env";
import { requestPath } from "./provider";
import { threadObjectName } from "./runtime";
export { ExoProvider, ExoThread } from "./provider";
export { ExoSandbox } from "./sandbox";
export { ExoEgress, proxyEgress } from "./network";

declare global {
  namespace Cloudflare {
    interface GlobalProps {
      mainModule: typeof import("./index");
      durableNamespaces: "ExoProvider" | "ExoThread" | "ExoSandbox";
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
    if (env.ACCESS_AUD !== undefined) {
      if (!env.ACCESS_AUD || ctx.access?.aud !== env.ACCESS_AUD)
        return new Response("Cloudflare Access required", { status: 403 });
      // Access context does not propagate to Durable Objects. Use the trusted
      // binding after verification, never a caller-supplied identity header.
    } else if (
      !env.EXO_TOKEN ||
      request.headers.get("authorization") !== `Bearer ${env.EXO_TOKEN}`
    )
      return new Response("Unauthorized", {
        status: 401,
        headers: { "WWW-Authenticate": 'Bearer realm="exo"' },
      });
    try {
      const path = requestPath(url);
      if (
        path[0] === "agent" &&
        path[2] === "thread" &&
        (path[4] === "turn" || (path[4] === "event" && path[5] === "watch"))
      ) {
        let name: string;
        try {
          name = threadObjectName(env.ACCOUNT_ID, path[1], path[3]);
        } catch (error) {
          return Response.json({ error: String(error) }, { status: 400 });
        }
        return await env.THREADS.getByName(name).handleRequest(request);
      }
      return await env.PROVIDERS.getByName(env.ACCOUNT_ID).handleRequest(
        request,
      );
    } catch (error) {
      return Response.json(
        { error: error instanceof Error ? error.message : String(error) },
        { status: error instanceof URIError ? 400 : 500 },
      );
    }
  },
} satisfies ExportedHandler<Env>;
