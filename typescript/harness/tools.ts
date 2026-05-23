import type {
  EventData,
  JsonObject,
  JsonValue,
  PendingToolCall,
  ToolDefinition,
  ToolResult,
  TurnContext,
} from "./index";

export type HarnessToolSource = "built_in" | "library" | "agent";

export interface ToolExecutionContext {
  readonly context: TurnContext;
  readonly toolCallId?: string;
}

export interface ToolHandler {
  execute(
    args: JsonObject,
    execution: ToolExecutionContext,
  ): Promise<ToolResult>;
}

export interface ToolInstance {
  definition: ToolDefinition;
  source: HarnessToolSource;
  handler: ToolHandler;
}

export interface ToolInitializationContext {
  readonly context: TurnContext;
  readonly source: HarnessToolSource;
}

export interface Tool {
  definition: ToolDefinition;
  initializationParameters: JsonValue;
  initialize(
    args: JsonObject,
    initialization: ToolInitializationContext,
  ): Promise<ToolHandler> | ToolHandler;
}

export class HarnessToolRegistry {
  private readonly tools = new Map<string, ToolInstance>();

  constructor(private readonly context: TurnContext) {}

  register(tool: ToolInstance): this {
    const { name } = tool.definition;
    if (this.tools.has(name)) {
      throw new Error(`tool is already registered: ${name}`);
    }
    this.tools.set(name, tool);
    return this;
  }

  definitions(): ToolDefinition[] {
    return [...this.tools.values()].map((tool) => tool.definition);
  }

  get(name: string): ToolInstance | undefined {
    return this.tools.get(name);
  }

  async executePending(toolCalls: PendingToolCall[]): Promise<EventData[]> {
    const events: EventData[] = [];
    for (const toolCall of toolCalls) {
      const result = await this.executeToolCall(toolCall);
      events.push(toolResultEvent(toolCall.toolCallId, result));
    }
    return events;
  }

  private async executeToolCall(
    toolCall: PendingToolCall,
  ): Promise<ToolResult> {
    const tool = this.tools.get(toolCall.request.functionName);
    if (!tool) {
      throw new Error(
        `tool execution is not configured for ${toolCall.request.functionName}`,
      );
    }
    if (this.context.streaming) {
      await this.context.stream.toolCall({
        toolCallId: toolCall.toolCallId,
        toolName: toolCall.request.functionName,
        arguments: toolCall.request.arguments,
      });
    }
    const result = await tool.handler.execute(toolCall.request.arguments, {
      context: this.context,
      toolCallId: toolCall.toolCallId,
    });
    if (this.context.streaming) {
      await this.context.stream.toolResult({
        toolCallId: toolCall.toolCallId,
        result,
      });
    }
    return result;
  }
}

export function createToolRegistry(context: TurnContext): HarnessToolRegistry {
  return new HarnessToolRegistry(context);
}

export async function initializeTool(
  tool: Tool,
  source: HarnessToolSource,
  initializationArgs: JsonObject,
  context: TurnContext,
): Promise<ToolInstance> {
  validateJsonSchema(
    tool.initializationParameters,
    initializationArgs,
    "tool initialization",
  );
  return {
    definition: tool.definition,
    source,
    handler: await tool.initialize(initializationArgs, {
      context,
      source,
    }),
  };
}

function validateJsonSchema(
  schema: JsonValue,
  value: JsonValue,
  path: string,
): void {
  if (!isRecord(schema)) {
    return;
  }
  const type = schema.type;
  if (type !== undefined && !matchesJsonSchemaType(type, value)) {
    throw new Error(
      `${path} does not match schema type ${formatSchemaType(type)}`,
    );
  }
  if (type !== "object" || !isRecord(value)) {
    return;
  }
  const properties = isRecord(schema.properties) ? schema.properties : {};
  const required = Array.isArray(schema.required) ? schema.required : [];
  for (const requiredKey of required) {
    if (typeof requiredKey === "string" && !(requiredKey in value)) {
      throw new Error(`${path}.${requiredKey} is required`);
    }
  }
  if (schema.additionalProperties === false) {
    for (const key of Object.keys(value)) {
      if (!(key in properties)) {
        throw new Error(`${path}.${key} is not allowed`);
      }
    }
  }
  for (const [key, propertySchema] of Object.entries(properties)) {
    if (key in value) {
      validateJsonSchema(
        propertySchema as JsonValue,
        value[key],
        `${path}.${key}`,
      );
    }
  }
}

function matchesJsonSchemaType(type: JsonValue, value: JsonValue): boolean {
  if (Array.isArray(type)) {
    return type.some((candidate) => matchesJsonSchemaType(candidate, value));
  }
  if (type === "null") {
    return value === null;
  }
  if (type === "array") {
    return Array.isArray(value);
  }
  if (type === "object") {
    return isRecord(value);
  }
  if (type === "string") {
    return typeof value === "string";
  }
  if (type === "number") {
    return typeof value === "number";
  }
  if (type === "boolean") {
    return typeof value === "boolean";
  }
  return true;
}

function formatSchemaType(type: JsonValue): string {
  return Array.isArray(type) ? type.map(String).join(" | ") : String(type);
}

function isRecord(value: JsonValue): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function toolResultEvent(toolCallId: string, result: ToolResult): EventData {
  return {
    type: "tool_result",
    tool_call_id: toolCallId,
    result,
  };
}
