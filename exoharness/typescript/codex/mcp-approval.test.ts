import { describe, expect, it, vi } from "vitest";
import {
  type JsonObject,
  type NativeMcpServer,
  type PendingToolCall,
} from "../harness";
import { authorizeMcpElicitation } from "./mcp-approval";

const mcpServers: NativeMcpServer[] = [
  {
    name: "notion",
    url: "https://mcp.notion.com/mcp",
    environmentVariable: null,
    disabledTools: [],
    tools: ["pages", "teams"].map((name) => ({
      name,
      exposedName: `exo_mcp__notion__${name}`,
    })),
  },
];

function call(
  id: string,
  name: string,
  args: JsonObject = {},
): PendingToolCall {
  return {
    toolCallId: id,
    request: { functionName: `exo_mcp__notion__${name}`, arguments: args },
  };
}

function elicitation(name: string, args: JsonObject = {}) {
  return {
    serverName: "notion",
    mode: "form",
    message: `Allow the notion MCP server to run tool "${name}"?`,
    _meta: { codex_approval_kind: "mcp_tool_call", tool_params: args },
  };
}

describe("Codex MCP elicitation approvals", () => {
  it("keeps overlapping allow and deny decisions independent", async () => {
    const pages = call("pages-call", "pages");
    const teams = call("teams-call", "teams");
    const calls = new Map([
      [pages.toolCallId, pages],
      [teams.toolCallId, teams],
    ]);
    let allowPages = () => {};
    const decision = new Promise<void>((resolve) => {
      allowPages = resolve;
    });
    const authorizeTool = vi.fn(async (request) => {
      if (request.functionName === pages.request.functionName) return decision;
      throw new Error("denied");
    });
    const context = { mcpServers, authorizeTool };
    const allowed = authorizeMcpElicitation(
      context,
      elicitation("pages"),
      calls,
    );
    const denied = authorizeMcpElicitation(
      context,
      elicitation("teams"),
      calls,
    );
    await expect(denied).resolves.toEqual({ action: "decline", content: null });
    expect(authorizeTool.mock.calls.map(([request]) => request)).toEqual([
      pages.request,
      teams.request,
    ]);
    allowPages();
    await expect(allowed).resolves.toEqual({ action: "accept", content: {} });
    expect(calls.size).toBe(0);
    await expect(
      authorizeMcpElicitation(context, elicitation("pages"), calls),
    ).resolves.toEqual({ action: "decline", content: null });
    expect(authorizeTool).toHaveBeenCalledTimes(2);
  });

  it("matches the actual arguments when approvals arrive out of order", async () => {
    const first = call("first", "pages", { query: "one", limit: 5 });
    const second = call("second", "pages", { query: "two", limit: 5 });
    const calls = new Map([
      [first.toolCallId, first],
      [second.toolCallId, second],
    ]);
    const authorizeTool = vi.fn(async () => {});
    await expect(
      authorizeMcpElicitation(
        { mcpServers, authorizeTool },
        elicitation("pages", { limit: 5, query: "two" }),
        calls,
      ),
    ).resolves.toEqual({ action: "accept", content: {} });
    expect(authorizeTool).toHaveBeenCalledWith(second.request);
    expect([...calls.keys()]).toEqual(["first"]);
  });

  it.each([
    { serverName: "other" },
    { mode: "url" },
    { _meta: {} },
    {
      _meta: {
        codex_approval_kind: "mcp_tool_call",
        tool_params: { unexpected: true },
      },
    },
    { message: "Allow a different operation?" },
    { message: 'Allow the notion MCP server to run tool "unknown"?' },
  ])("does not authorize unrelated elicitations: %j", async (changed) => {
    const pending = call("known", "pages");
    const calls = new Map([[pending.toolCallId, pending]]);
    const authorizeTool = vi.fn(async () => {});
    const result = await authorizeMcpElicitation(
      { mcpServers, authorizeTool },
      { ...elicitation("pages"), ...changed },
      calls,
    );
    expect(result).not.toEqual({ action: "accept", content: {} });
    expect(authorizeTool).not.toHaveBeenCalled();
    expect(calls.size).toBe(1);
  });
});
