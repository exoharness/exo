import { describe, expect, it } from "vitest";

import { validateAdapterSource } from "./adapter-tools";
import {
  createToolRegistry,
  registerAdapterTools,
  type JsonObject,
  type TurnContext,
} from "./index";

describe("validateAdapterSource", () => {
  it("requires built_in for irc and agent-cli adapters", () => {
    expect(() => validateAdapterSource("built_in", "irc")).not.toThrow();
    expect(() => validateAdapterSource("built_in", "agent-cli")).not.toThrow();
    expect(() => validateAdapterSource("library", "irc")).toThrow(
      "irc adapters must use source 'built_in'",
    );
    expect(() => validateAdapterSource("library", "agent-cli")).toThrow(
      "agent-cli adapters must use source 'built_in'",
    );
  });

  it("requires library for whatsapp, signal, and discord adapters", () => {
    for (const type of ["whatsapp", "signal", "discord"]) {
      expect(() => validateAdapterSource("library", type)).not.toThrow();
      expect(() => validateAdapterSource("built_in", type)).toThrow(
        `${type} adapters must use source 'library'`,
      );
    }
  });
});

describe("create_adapter tool", () => {
  function registryWithSpy(executedRequests: JsonObject[]) {
    const context = {
      streaming: false,
      executeTool: async (request: {
        functionName: string;
        arguments: JsonObject;
      }) => {
        executedRequests.push({
          functionName: request.functionName,
          arguments: request.arguments,
        });
        return { ok: true };
      },
    } as unknown as TurnContext;
    const registry = createToolRegistry(context);
    registerAdapterTools(registry, ["create_adapter"]);
    return { context, registry };
  }

  it("forwards valid source/type combinations to the host tool", async () => {
    const executedRequests: JsonObject[] = [];
    const { context, registry } = registryWithSpy(executedRequests);
    const args = {
      name: "irc-bridge",
      source: "built_in",
      config: { type: "irc" },
    };

    await registry.get("create_adapter")!.handler.execute(args, { context });

    expect(executedRequests).toEqual([
      { functionName: "create_adapter", arguments: args },
    ]);
  });

  it("rejects mismatched source/type combinations before reaching the host", () => {
    const executedRequests: JsonObject[] = [];
    const { context, registry } = registryWithSpy(executedRequests);

    // The guard runs synchronously, before the host executeTool is invoked.
    expect(() =>
      registry.get("create_adapter")!.handler.execute(
        {
          name: "discord-bot",
          source: "built_in",
          config: { type: "discord" },
        },
        { context },
      ),
    ).toThrow("discord adapters must use source 'library'");
    expect(executedRequests).toEqual([]);
  });
});
