import type { JsonObject, ToolDefinition, TurnContext } from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";

export type AdapterToolName =
  | "create_adapter"
  | "list_adapters"
  | "disable_adapter"
  | "delete_adapter"
  | "send_adapter_message";

export function registerAdapterTools(
  registry: HarnessToolRegistry,
  names: AdapterToolName[] = [
    "create_adapter",
    "list_adapters",
    "disable_adapter",
    "delete_adapter",
    "send_adapter_message",
  ],
): void {
  const requested = new Set<AdapterToolName>(names);
  for (const tool of createAdapterToolInstances()) {
    if (requested.has(tool.definition.name as AdapterToolName)) {
      registry.register(tool);
    }
  }
}

function createAdapterToolInstances(): ToolInstance[] {
  return [
    createAdapterTool(),
    listAdaptersTool(),
    disableAdapterTool(),
    deleteAdapterTool(),
    sendAdapterMessageTool(),
  ];
}

function createAdapterTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "create_adapter",
      description:
        "Create and enable a long-running Exoclaw adapter for this conversation. Use source 'built_in' only with config type 'irc'. Use source 'library' with config type 'whatsapp' or 'signal' for shipped library adapters.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Stable adapter name using letters, numbers, dashes, or underscores.",
          },
          source: {
            type: "string",
            enum: ["built_in", "library"],
            description: "Adapter source.",
          },
          config: adapterConfigSchema(),
        },
        required: ["name", "source", "config"],
      },
    },
    handler: {
      execute(toolArgs, execution) {
        return execution.context.executeTool({
          functionName: "create_adapter",
          arguments: withConversationScope(
            execution.context,
            transformCreateAdapterArguments(toolArgs),
          ),
        });
      },
    },
  };
}

function listAdaptersTool(): ToolInstance {
  return hostTool({
    name: "list_adapters",
    description:
      "List adapters for this conversation. Disabled adapters are hidden unless includeDisabled is true.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        includeDisabled: {
          type: ["boolean", "null"],
          description: "Whether to include disabled adapters.",
        },
      },
      required: ["includeDisabled"],
    },
  });
}

function disableAdapterTool(): ToolInstance {
  return hostTool({
    name: "disable_adapter",
    description:
      "Disable an adapter for this conversation while preserving its event history.",
    parameters: adapterIdParameters(
      "Adapter id returned by create_adapter or list_adapters.",
    ),
  });
}

function deleteAdapterTool(): ToolInstance {
  return hostTool({
    name: "delete_adapter",
    description:
      "Permanently delete an adapter for this conversation, including its stored event history.",
    parameters: adapterIdParameters(
      "Adapter id returned by create_adapter or list_adapters.",
    ),
  });
}

function sendAdapterMessageTool(): ToolInstance {
  return hostTool({
    name: "send_adapter_message",
    description:
      "Send an explicit outbound message through an adapter. For IRC this sends PRIVMSG to the adapter channel. For WhatsApp, provide target as the chat id from the inbound message. For Signal, provide a username such as u:example.01, a phone number, UUID, or group id. Only call this when the user or conversation context makes the external side effect appropriate.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        adapterId: {
          type: "string",
          description:
            "Adapter id returned by create_adapter or list_adapters.",
        },
        text: {
          type: "string",
          description: "Message text to send through the adapter.",
        },
        target: {
          type: ["string", "null"],
          description:
            "External destination for adapters that need one. Use the target from the inbound wakeup when available; WhatsApp requires a chat id and Signal requires a username/phone/UUID/group id.",
        },
        attachments: {
          anyOf: [
            {
              type: "array",
              items: {
                type: "object",
                additionalProperties: false,
                properties: {
                  kind: {
                    type: "string",
                    enum: ["image", "video", "audio", "document"],
                    description:
                      "Attachment kind. Rich attachments are currently supported by the WhatsApp and Signal adapters.",
                  },
                  path: {
                    type: ["string", "null"],
                    description:
                      "Local host path visible to the adapter worker. Do not use sandbox paths here. Specify exactly one of path, url, data, or sandboxPath.",
                  },
                  url: {
                    type: ["string", "null"],
                    description:
                      "HTTPS URL for the media file. Specify exactly one of path, url, data, or sandboxPath.",
                  },
                  data: {
                    type: ["string", "null"],
                    description:
                      "Base64 media bytes, or a data: URL. Prefer sandboxPath for files created inside the sandbox; large inline data may be truncated.",
                  },
                  sandboxPath: {
                    type: ["string", "null"],
                    description:
                      "Path to a media file inside the active Exoclaw sandbox. Use this for files generated by shell commands in the sandbox.",
                  },
                  mimeType: {
                    type: ["string", "null"],
                    description:
                      "Optional MIME type. Required for WhatsApp documents and recommended for audio.",
                  },
                  fileName: {
                    type: ["string", "null"],
                    description:
                      "Optional file name. Required for WhatsApp documents.",
                  },
                },
                required: [
                  "kind",
                  "path",
                  "url",
                  "data",
                  "sandboxPath",
                  "mimeType",
                  "fileName",
                ],
              },
            },
            { type: "null" },
          ],
          description:
            "Optional rich media attachments. Use null for text-only messages.",
        },
      },
      required: ["adapterId", "text", "target", "attachments"],
    },
  });
}

function hostTool(args: {
  name: AdapterToolName;
  functionName?: string;
  description: string;
  parameters: ToolDefinition["parameters"];
}): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: args.name,
      description: args.description,
      parameters: args.parameters,
    },
    handler: {
      execute(toolArgs, execution) {
        return execution.context.executeTool({
          functionName: args.functionName ?? args.name,
          arguments: withConversationScope(execution.context, toolArgs),
        });
      },
    },
  };
}

function adapterConfigSchema(): ToolDefinition["parameters"] {
  return {
    anyOf: [
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["irc"] },
          server: { type: "string" },
          port: { type: "number" },
          tls: { type: "boolean" },
          nick: { type: "string" },
          username: { type: "string" },
          realname: { type: "string" },
          channel: { type: "string" },
          passwordSecretId: { type: ["string", "null"] },
          trigger: {
            type: "string",
            enum: ["mention", "all_messages"],
            description:
              "Wake policy. Use mention unless the user explicitly wants every channel message.",
          },
        },
        required: [
          "type",
          "server",
          "port",
          "tls",
          "nick",
          "username",
          "realname",
          "channel",
          "passwordSecretId",
          "trigger",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["signal"] },
          account: {
            type: ["string", "null"],
            description:
              "Optional local signal-cli account identifier. Use null to have the worker start signal-cli link and discover the linked account.",
          },
          deviceName: {
            type: ["string", "null"],
            description:
              "Optional linked-device name when account is null. Use null for Exoclaw.",
          },
          signalCliCommand: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional command argv for signal-cli. Use null for ['signal-cli'].",
          },
          configDir: {
            type: ["string", "null"],
            description:
              "Optional signal-cli config directory. Use null for the adapter state directory.",
          },
          trigger: {
            type: "string",
            enum: ["all_messages", "contacts_only"],
            description:
              "Wake policy. Use all_messages for the MVP unless allowedContacts is set.",
          },
          allowedContacts: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional list of Signal usernames, phone numbers, UUIDs, or group ids to wake on. Use null to allow all inbound messages.",
          },
        },
        required: [
          "type",
          "account",
          "deviceName",
          "signalCliCommand",
          "configDir",
          "trigger",
          "allowedContacts",
        ],
      },
      {
        type: "object",
        additionalProperties: false,
        properties: {
          type: { type: "string", enum: ["whatsapp"] },
          authDir: {
            type: ["string", "null"],
            description:
              "Optional directory for Baileys auth state. Use null for the default under .exo.",
          },
          trigger: {
            type: "string",
            enum: ["all_messages", "contacts_only"],
            description:
              "Wake policy. Use all_messages for the MVP unless the user wants to ignore groups.",
          },
          allowedChats: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional list of WhatsApp chat ids to wake on. Use null to allow all chats permitted by trigger.",
          },
          workerCommand: {
            anyOf: [
              { type: "array", items: { type: "string" } },
              { type: "null" },
            ],
            description:
              "Optional command argv for the worker. Use null for the bundled Baileys worker.",
          },
        },
        required: [
          "type",
          "authDir",
          "trigger",
          "allowedChats",
          "workerCommand",
        ],
      },
    ],
  } as ToolDefinition["parameters"];
}

function transformCreateAdapterArguments(args: JsonObject): JsonObject {
  const config = objectField(args, "config");
  validateAdapterSource(
    stringField(args, "source"),
    stringField(config, "type"),
  );
  return {
    ...args,
    config: transformAdapterConfig(config),
  };
}

function validateAdapterSource(source: string, type: string): void {
  if (type === "irc" && source !== "built_in") {
    throw new Error("IRC adapters must use source 'built_in'");
  }
  if ((type === "whatsapp" || type === "signal") && source !== "library") {
    throw new Error(`${type} adapters must use source 'library'`);
  }
}

function transformAdapterConfig(config: JsonObject): JsonObject {
  const type = stringField(config, "type");
  if (type === "irc") {
    const passwordSecretId = nullableStringField(config, "passwordSecretId");
    return {
      adapterType: "irc",
      workerCommand: ["pnpm", "tsx", "examples/exoclaw/adapters/irc/worker.ts"],
      initialization: {
        server: stringField(config, "server"),
        port: numberField(config, "port"),
        tls: booleanField(config, "tls"),
        nick: stringField(config, "nick"),
        username: stringField(config, "username"),
        realname: stringField(config, "realname"),
        channel: stringField(config, "channel"),
        trigger: stringField(config, "trigger"),
      },
      stateDir: null,
      secretEnv:
        passwordSecretId === null
          ? []
          : [{ env: "EXO_IRC_PASSWORD", secretId: passwordSecretId }],
    };
  }
  if (type === "whatsapp") {
    return {
      adapterType: "whatsapp",
      workerCommand: [
        "pnpm",
        "tsx",
        "examples/exoclaw/adapters/whatsapp/worker.ts",
      ],
      initialization: {
        authDir: nullableStringField(config, "authDir"),
        trigger: stringField(config, "trigger"),
        allowedChats: nullableStringArrayField(config, "allowedChats"),
      },
      stateDir: null,
      secretEnv: [],
    };
  }
  if (type === "signal") {
    return {
      adapterType: "signal",
      workerCommand: [
        "pnpm",
        "tsx",
        "examples/exoclaw/adapters/signal/worker.ts",
      ],
      initialization: {
        account: nullableStringField(config, "account"),
        deviceName: nullableStringField(config, "deviceName"),
        signalCliCommand: nullableStringArrayField(config, "signalCliCommand"),
        configDir: nullableStringField(config, "configDir"),
        trigger: stringField(config, "trigger"),
        allowedContacts: nullableStringArrayField(config, "allowedContacts"),
      },
      stateDir: null,
      secretEnv: [],
    };
  }
  return config;
}

function adapterIdParameters(
  description: string,
): ToolDefinition["parameters"] {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      adapterId: {
        type: "string",
        description,
      },
    },
    required: ["adapterId"],
  };
}

function withConversationScope(
  context: TurnContext,
  args: JsonObject,
): JsonObject {
  const { agent, conversation } = context.exoharness.current;
  return {
    ...args,
    agentId: agent.record.id,
    conversationId: conversation.record.id,
  };
}

function stringField(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`adapter argument ${name} must be a non-empty string`);
  }
  return value;
}

function nullableStringField(args: JsonObject, name: string): string | null {
  const value = args[name];
  if (value === null || value === undefined) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(
      `adapter argument ${name} must be null or a non-empty string`,
    );
  }
  return value;
}

function numberField(args: JsonObject, name: string): number {
  const value = args[name];
  if (typeof value !== "number") {
    throw new Error(`adapter argument ${name} must be a number`);
  }
  return value;
}

function booleanField(args: JsonObject, name: string): boolean {
  const value = args[name];
  if (typeof value !== "boolean") {
    throw new Error(`adapter argument ${name} must be a boolean`);
  }
  return value;
}

function objectField(args: JsonObject, name: string): JsonObject {
  const value = args[name];
  if (!isRecord(value)) {
    throw new Error(`adapter argument ${name} must be an object`);
  }
  return value;
}

function nullableStringArrayField(
  args: JsonObject,
  name: string,
): string[] | null {
  const value = args[name];
  if (value === null || value === undefined) {
    return null;
  }
  if (
    !Array.isArray(value) ||
    !value.every((item) => typeof item === "string")
  ) {
    throw new Error(
      `adapter argument ${name} must be null or an array of strings`,
    );
  }
  return value;
}

function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
