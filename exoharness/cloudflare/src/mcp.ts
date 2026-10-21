import {
  Client,
  StreamableHTTPClientTransport,
} from "@modelcontextprotocol/client";
import { CfWorkerJsonSchemaValidator } from "@modelcontextprotocol/client/validators/cf-worker";
import type { Runtime } from "./runtime";

export type McpCommand =
  | { type: "connect"; id: string; url: string }
  | { type: "list" | "close"; client: Client }
  | {
      type: "call";
      client: Client;
      name: string;
      arguments: Record<string, unknown>;
    };
export type McpHttpRequest = {
  id: string;
  url: string;
  method: string;
  headers: [string, string][];
  body: string | null;
};

export async function mcp(
  command: McpCommand,
  runtime: Runtime,
  signal: AbortSignal,
): Promise<unknown> {
  switch (command.type) {
    case "connect": {
      const client = new Client(
        { name: "exo", version: "0.1.0" },
        {
          jsonSchemaValidator: new CfWorkerJsonSchemaValidator(),
        },
      );
      const transport = new StreamableHTTPClientTransport(
        new URL(command.url),
        {
          fetch: async (input, init) => {
            const request = new Request(input, init);
            const result = await runtime.call<{ response: Response }>(
              {
                type: "mcp_http",
                request: {
                  id: command.id,
                  url: request.url,
                  method: request.method,
                  headers: [...request.headers],
                  body: request.body ? await request.text() : null,
                },
              },
              request.signal,
            );
            return result.response;
          },
        },
      );
      try {
        await client.connect(transport, { signal, timeout: 60_000 });
        signal.throwIfAborted();
        return client;
      } catch (error) {
        await client.close();
        throw error;
      }
    }
    case "list":
      // The SDK aggregates all pages when no cursor is supplied.
      return (
        await command.client.listTools(undefined, { signal, timeout: 60_000 })
      ).tools;
    case "call":
      return command.client.callTool(
        { name: command.name, arguments: command.arguments },
        { signal, timeout: 300_000 },
      );
    case "close":
      await command.client.close();
      return null;
  }
}
