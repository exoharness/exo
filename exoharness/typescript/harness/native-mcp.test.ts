import { describe, expect, it } from "vitest";
import { type NativeMcpServer } from "./index";
import { claudeToolName, codexMcpToolName } from "./native-mcp";

const servers: NativeMcpServer[] = [
  {
    name: "notion",
    url: "https://mcp.notion.com/mcp",
    environmentVariable: null,
    disabledTools: ["delete_page"],
    tools: [
      { name: "create_page", exposedName: "exo_mcp__notion__create_page" },
    ],
  },
];

describe("native MCP tool names", () => {
  it.each([
    ["notion", "read_mcp_resource"],
    ["notion", "list_mcp_resources"],
    ["notion", "list_mcp_resource_templates"],
    ["codex", "list_mcp_resources"],
    ["codex", "list_mcp_resource_templates"],
  ])(
    "records Codex resource operations without requiring a server tool: %s/%s",
    (server, name) => {
      expect(codexMcpToolName(servers, server, name)).toBe(`codex.${name}`);
    },
  );

  it("maps enabled server tools to the shared approval name", () => {
    expect(codexMcpToolName(servers, "notion", "create_page")).toBe(
      "exo_mcp__notion__create_page",
    );
    expect(claudeToolName(servers, "mcp__notion__create_page")).toBe(
      "exo_mcp__notion__create_page",
    );
  });

  it("still rejects disabled tools and unconfigured servers", () => {
    expect(() => codexMcpToolName(servers, "notion", "delete_page")).toThrow(
      "MCP tool is not enabled",
    );
    expect(() => codexMcpToolName(servers, "other", "create_page")).toThrow(
      "MCP tool is not enabled",
    );
    expect(claudeToolName(servers, "mcp__notion__delete_page")).toBeUndefined();
    expect(
      claudeToolName(servers, "mcp__new-server__new-tool"),
    ).toBeUndefined();
  });
});
