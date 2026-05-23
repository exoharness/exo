import { describe, expect, it } from "vitest";

import {
  buildShellToolDefinitions,
  createShellToolInstance,
  createToolRegistry,
  initializeTool,
  registerBuiltInTools,
  registerAgentToolsFromManifest,
  registerLibraryToolsFromManifest,
  registerToolsFromManifest,
  toolResultEvent,
  type EventData,
  type JsonObject,
  type ToolExecutionContext,
  type ToolInstance,
  type ToolResult,
  type TurnContext,
} from "./index";
import ircTool from "../../examples/typescript/tools/irc";
import uppercaseTool from "../../examples/typescript/tools/uppercase";

describe("HarnessToolRegistry", () => {
  it("returns registered tool definitions", () => {
    const context = fakeTurnContext();
    const tool = fakeTool("echo", async (args) => args);
    const registry = createToolRegistry(context).register(tool);

    expect(registry.definitions()).toEqual([tool.definition]);
    expect(registry.get("echo")).toBe(tool);
  });

  it("rejects duplicate tool names", () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context).register(
      fakeTool("echo", async (args) => args),
    );

    expect(() =>
      registry.register(fakeTool("echo", async (args) => args)),
    ).toThrow("tool is already registered: echo");
  });

  it("executes pending tool calls and returns tool result events", async () => {
    const context = fakeTurnContext();
    const executionContexts: ToolExecutionContext[] = [];
    const registry = createToolRegistry(context).register(
      fakeTool("echo", async (args, execution) => {
        executionContexts.push(execution);
        return { echoed: args.value };
      }),
    );

    const events = await registry.executePending([
      {
        toolCallId: "call_1",
        request: {
          functionName: "echo",
          arguments: { value: "hello" },
        },
      },
    ]);

    expect(events).toEqual([
      toolResultEvent("call_1", {
        echoed: "hello",
      }),
    ]);
    expect(executionContexts).toHaveLength(1);
    expect(executionContexts[0].context).toBe(context);
    expect(executionContexts[0].toolCallId).toBe("call_1");
  });

  it("emits stream events around tool execution when streaming", async () => {
    const streamEvents: EventData[] = [];
    const context = fakeTurnContext({
      streaming: true,
      streamEvents,
    });
    const registry = createToolRegistry(context).register(
      fakeTool("echo", async (args) => ({ echoed: args.value })),
    );

    await registry.executePending([
      {
        toolCallId: "call_1",
        request: {
          functionName: "echo",
          arguments: { value: "hello" },
        },
      },
    ]);

    expect(streamEvents).toEqual([
      {
        type: "tool_call_streamed",
        toolCallId: "call_1",
        toolName: "echo",
        arguments: { value: "hello" },
      },
      {
        type: "tool_result_streamed",
        toolCallId: "call_1",
        result: { echoed: "hello" },
      },
    ]);
  });

  it("throws for unregistered tools", async () => {
    const registry = createToolRegistry(fakeTurnContext());

    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "missing",
            arguments: {},
          },
        },
      ]),
    ).rejects.toThrow("tool execution is not configured for missing");
  });
});

describe("shell built-in tool", () => {
  it("builds the existing shell tool definition shape", () => {
    expect(
      buildShellToolDefinitions({
        enableNetworking: false,
        shellProgram: "/bin/bash",
        mounts: [],
      }),
    ).toEqual([
      {
        name: "shell",
        description: "Run a shell command using /bin/bash.",
        parameters: {
          type: "object",
          additionalProperties: false,
          properties: {
            command: {
              type: "string",
              description: "Shell command to execute.",
            },
          },
          required: ["command"],
        },
      },
    ]);
  });

  it("omits the shell definition when shell is disabled", () => {
    expect(
      buildShellToolDefinitions({
        enableNetworking: false,
        shellProgram: null,
        mounts: [],
      }),
    ).toEqual([]);
  });

  it("delegates shell execution to the host tool path", async () => {
    const executedRequests: JsonObject[] = [];
    const context = fakeTurnContext({
      executeTool: async (request) => {
        executedRequests.push({
          functionName: request.functionName,
          arguments: request.arguments,
        });
        return {
          stdout: "ok\n",
          stderr: "",
          exit_code: 0,
        };
      },
    });
    const shell = createShellToolInstance({
      enableNetworking: false,
      shellProgram: "/bin/bash",
      mounts: [],
    });

    expect(shell).not.toBeNull();
    const result = await shell!.handler.execute(
      { command: "echo ok" },
      {
        context,
        toolCallId: "call_1",
      },
    );

    expect(executedRequests).toEqual([
      {
        functionName: "shell",
        arguments: { command: "echo ok" },
      },
    ]);
    expect(result).toEqual({
      stdout: "ok\n",
      stderr: "",
      exit_code: 0,
    });
  });

  it("registers requested built-in tools", () => {
    const context = fakeTurnContext({
      conversationConfig: {
        enableNetworking: false,
        shellProgram: "/bin/bash",
        mounts: [],
      },
    });
    const registry = createToolRegistry(context);

    registerBuiltInTools(registry, context, ["shell"]);

    expect(registry.definitions()).toEqual(
      buildShellToolDefinitions(context.conversationConfig),
    );
  });
});

describe("library tool modules", () => {
  it("initializes, registers, and executes a direct TypeScript tool", async () => {
    const context = fakeTurnContext();
    const tool = await initializeTool(
      uppercaseTool,
      "library",
      {
        prefix: "result: ",
      },
      context,
    );
    const registry = createToolRegistry(context).register(tool);

    expect(registry.definitions()).toEqual([uppercaseTool.definition]);
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "uppercase",
            arguments: {
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      toolResultEvent("call_1", {
        text: "result: HELLO",
      }),
    ]);
  });

  it("initializes and executes the demo IRC tool in dry-run mode", async () => {
    const context = fakeTurnContext();
    const tool = await initializeTool(
      ircTool,
      "library",
      {
        server: "irc.example.test",
        port: 6697,
        nick: "exo-agent",
        username: "exo",
        realname: "Exo Agent",
        tls: true,
        dryRun: true,
        passwordSecretId: null,
      },
      context,
    );
    const registry = createToolRegistry(context).register(tool);

    expect(registry.definitions()).toEqual([ircTool.definition]);
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "irc_send_message",
            arguments: {
              channel: "#exo",
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      toolResultEvent("call_1", {
        ok: true,
        dryRun: true,
        registered: false,
        joined: false,
        server: "irc.example.test",
        channel: "#exo",
      }),
    ]);
  });
});

describe("agent tool loading", () => {
  it("loads and registers library tools from a manifest", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerLibraryToolsFromManifest(registry, context, {
      tools: [
        {
          modulePath: uppercaseToolModulePath(),
          initialization: {
            prefix: "library: ",
          },
        },
      ],
    });

    expect(registry.get("uppercase")?.source).toBe("library");
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "uppercase",
            arguments: {
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      toolResultEvent("call_1", {
        text: "library: HELLO",
      }),
    ]);
  });

  it("loads and registers agent tools from a manifest", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerAgentToolsFromManifest(registry, context, {
      tools: [
        {
          modulePath: uppercaseToolModulePath(),
          initialization: {
            prefix: "agent: ",
          },
        },
      ],
    });

    expect(registry.definitions()).toEqual([uppercaseTool.definition]);
    expect(registry.get("uppercase")?.source).toBe("agent");
    await expect(
      registry.executePending([
        {
          toolCallId: "call_1",
          request: {
            functionName: "uppercase",
            arguments: {
              text: "hello",
            },
          },
        },
      ]),
    ).resolves.toEqual([
      toolResultEvent("call_1", {
        text: "agent: HELLO",
      }),
    ]);
  });

  it("loads tools through the generic source-aware manifest path", async () => {
    const context = fakeTurnContext();
    const registry = createToolRegistry(context);

    await registerToolsFromManifest(
      registry,
      context,
      {
        tools: [
          {
            modulePath: uppercaseToolModulePath(),
            initialization: {
              prefix: "generic: ",
            },
          },
        ],
      },
      "library",
    );

    expect(registry.get("uppercase")?.source).toBe("library");
  });

  it("rejects agent tool modules without a default Tool export", async () => {
    const registry = createToolRegistry(fakeTurnContext());

    await expect(
      registerAgentToolsFromManifest(registry, fakeTurnContext(), {
        tools: [
          {
            modulePath: "data:text/javascript,export const value = 1;",
            initialization: {},
          },
        ],
      }),
    ).rejects.toThrow("agent tool module must default export a Tool");
  });

  it("rejects invalid agent tool initialization", async () => {
    const registry = createToolRegistry(fakeTurnContext());

    await expect(
      registerAgentToolsFromManifest(registry, fakeTurnContext(), {
        tools: [
          {
            modulePath: uppercaseToolModulePath(),
            initialization: {},
          },
        ],
      }),
    ).rejects.toThrow("tool initialization.prefix is required");
  });
});

function fakeTool(
  name: string,
  execute: (
    args: JsonObject,
    execution: ToolExecutionContext,
  ) => Promise<ToolResult>,
): ToolInstance {
  return {
    source: "library",
    definition: {
      name,
      description: `Fake ${name} tool.`,
      parameters: {
        type: "object",
        additionalProperties: true,
      },
    },
    handler: {
      execute,
    },
  };
}

function uppercaseToolModulePath(): string {
  return new URL(
    "../../examples/typescript/tools/uppercase.ts",
    import.meta.url,
  ).href;
}

function fakeTurnContext(
  options: {
    streaming?: boolean;
    streamEvents?: EventData[];
    executeTool?: TurnContext["executeTool"];
    conversationConfig?: TurnContext["conversationConfig"];
  } = {},
): TurnContext {
  const streamEvents = options.streamEvents ?? [];
  return {
    agentConfig: {
      instructions: [],
      harness: "typescript",
      typescript: null,
      libraryTools: [],
      sandboxImage: null,
      enableNetworking: false,
      model: "test-model",
      maxOutputTokens: null,
      maxToolRoundTrips: null,
      braintrust: null,
    },
    conversationConfig: options.conversationConfig ?? {
      enableNetworking: false,
      shellProgram: null,
      mounts: [],
    },
    request: {
      input: [],
      sessionId: null,
    },
    streaming: options.streaming ?? false,
    braintrustParent: null,
    exoharness: {
      current: {
        agent: {},
        conversation: {},
        turn: {},
      },
    },
    executeTool: options.executeTool ?? (async () => null),
    async startSandboxProcess() {
      throw new Error("not implemented");
    },
    async executePendingTools() {
      return [];
    },
    stream: {
      async firstChunk(ttftMs: number) {
        streamEvents.push({ type: "first_chunk_streamed", ttftMs });
      },
      async text(text: string) {
        streamEvents.push({ type: "text_streamed", text });
      },
      async toolCall(args: {
        toolCallId: string;
        toolName: string;
        arguments: JsonObject;
      }) {
        streamEvents.push({
          type: "tool_call_streamed",
          ...args,
        });
      },
      async toolResult(args: { toolCallId: string; result: ToolResult }) {
        streamEvents.push({
          type: "tool_result_streamed",
          ...args,
        });
      },
    },
  } as unknown as TurnContext;
}
