import { type NativeMcpServer } from "./index";

export function codexMcpToolName(
  servers: NativeMcpServer[],
  server: string,
  name: string,
) {
  if (
    name === "list_mcp_resources" ||
    name === "list_mcp_resource_templates" ||
    name === "read_mcp_resource"
  ) {
    return `codex.${name}`;
  }
  const tool = servers
    .find((entry) => entry.name === server)
    ?.tools.find((entry) => entry.name === name);
  if (!tool) throw new Error(`MCP tool is not enabled: ${server}/${name}`);
  return tool.exposedName;
}

export function claudeToolName(
  servers: NativeMcpServer[],
  name: string,
): string | undefined {
  if (!name.startsWith("mcp__")) return `claude.${name}`;
  for (const server of servers) {
    for (const tool of server.tools) {
      if (name === `mcp__${server.name}__${tool.name}`) return tool.exposedName;
    }
  }
}
