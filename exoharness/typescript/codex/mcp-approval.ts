import { isDeepStrictEqual } from "node:util";
import {
  type JsonValue,
  type PendingToolCall,
  type TurnContext,
} from "../harness";
import { asRecord } from "../model-runtime/shared";

export async function authorizeMcpElicitation(
  context: Pick<TurnContext, "mcpServers" | "authorizeTool">,
  params: Record<string, unknown>,
  calls: Map<string, PendingToolCall>,
): Promise<JsonValue | undefined> {
  const meta = asRecord(params._meta);
  if (params.mode !== "form" || meta.codex_approval_kind !== "mcp_tool_call")
    return undefined;
  const server = context.mcpServers.find(
    (server) => server.name === params.serverName,
  );
  if (!server) return undefined;
  for (const [id, call] of calls) {
    const tool = server.tools.find(
      (tool) => tool.exposedName === call.request.functionName,
    );
    if (
      !tool ||
      params.message !==
        `Allow the ${server.name} MCP server to run tool "${tool.name}"?` ||
      !isDeepStrictEqual(meta.tool_params ?? {}, call.request.arguments)
    )
      continue;
    calls.delete(id);
    try {
      await context.authorizeTool(call.request);
      return { action: "accept", content: {} };
    } catch {
      return { action: "decline", content: null };
    }
  }
  return { action: "decline", content: null };
}
