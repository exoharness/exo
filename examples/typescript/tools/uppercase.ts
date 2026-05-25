// Example library tool used by the harness registry tests. No example harness
// exposes this tool to a model yet.

import type { JsonObject, Tool, ToolResult } from "@exo/harness/tool";

interface UppercaseConfig {
  prefix: string;
}

const uppercaseTool = {
  definition: {
    name: "uppercase",
    description: "Uppercase text and optionally prefix the result.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        text: {
          type: "string",
          description: "Text to uppercase.",
        },
      },
      required: ["text"],
    },
    outputSchema: {
      type: "object",
      additionalProperties: false,
      properties: {
        text: { type: "string" },
      },
      required: ["text"],
    },
  },
  initializationParameters: {
    type: "object",
    additionalProperties: false,
    properties: {
      prefix: {
        type: "string",
        description: "Prefix to prepend to each uppercase result.",
      },
    },
    required: ["prefix"],
  },
  initialize(args) {
    const config = parseConfig(args);
    return {
      async execute(args): Promise<ToolResult> {
        const text = stringArgument(args, "text");
        return {
          text: `${config.prefix}${text.toUpperCase()}`,
        };
      },
    };
  },
} satisfies Tool;

export default uppercaseTool;

function parseConfig(args: JsonObject): UppercaseConfig {
  return {
    prefix: stringArgument(args, "prefix"),
  };
}

function stringArgument(args: JsonObject, name: string): string {
  const value = args[name];
  if (typeof value !== "string") {
    throw new Error(`uppercase tool argument ${name} must be a string`);
  }
  return value;
}
