import type { Env } from "./env";
import { requestPath, staticAuthorization } from "./provider";
import { threadObjectName } from "./runtime";
export { ExoProvider, ExoThread } from "./provider";
export { ExoSandbox } from "./sandbox";
export { ExoEgress } from "./network";

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
    } else {
      const denied = staticAuthorization(env, request);
      if (denied) return denied;
    }
    try {
      const path = requestPath(request);
      if (
        path[0] === "agent" &&
        path[2] === "thread" &&
        (path[4] === "turn" || (path[4] === "event" && path[5] === "watch"))
      )
        return env.THREADS.getByName(
          threadObjectName(env.ACCOUNT_ID, path[1], path[3]),
        ).handleRequest(request);
      return env.PROVIDERS.getByName(env.ACCOUNT_ID).handleRequest(request);
    } catch (error) {
      return Response.json(
        { error: error instanceof Error ? error.message : String(error) },
        { status: 400 },
      );
    }
  },
} satisfies ExportedHandler<Env>;
