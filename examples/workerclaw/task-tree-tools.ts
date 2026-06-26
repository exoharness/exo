import { randomUUID } from "node:crypto";

import type {
  ArtifactVersion,
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  ToolDefinition,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

const TASK_TREE_ARTIFACT_PATH = "task-tree.json";

type TreeSnapshot = {
  rootRef: string;
  expectedDeliverables?: Array<{
    type: string;
    description: string;
    quantity: number;
  }>;
  nodes: Record<string, JsonObject>;
  updatedAt: string;
};

type BridgeEventPayload = {
  ok: true;
  bridgeEvent: JsonObject;
};

export function registerTaskTreeTools(registry: HarnessToolRegistry): void {
  for (const tool of createTaskTreeToolInstances()) {
    registry.register(tool);
  }
}

function createTaskTreeToolInstances(): ToolInstance[] {
  return [
    taskTreeInitTool(),
    taskTreeUpsertNodeTool(),
    taskTreeUpdateStatusTool(),
    reportDeliverableTool(),
    completeTaskTool(),
  ];
}

function taskTreeInitTool(): ToolInstance {
  return localTool({
    name: "task_tree_init",
    description:
      "Initialize the task tree after understanding the request. Call once early with objectives, sub-objectives, and TODO leaves. Tool results include a bridgeEvent payload for host integrations.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        rootRef: {
          type: "string",
          description: 'Stable ref for the root, usually "root".',
        },
        expectedDeliverables: {
          type: ["array", "null"],
          items: {
            type: "object",
            additionalProperties: false,
            properties: {
              type: { type: "string" },
              description: { type: "string" },
              quantity: { type: "number" },
            },
            required: ["type", "description", "quantity"],
          },
          description:
            "What the client should receive when the task completes.",
        },
        nodes: {
          type: "array",
          items: nodeDraftSchema(),
          description:
            "All non-root nodes: objectives (depth 1), sub-objectives (depth 2), TODOs (depth 3, isLeaf true).",
        },
      },
      required: ["rootRef", "expectedDeliverables", "nodes"],
    },
    buildEvent(args) {
      return bridgeEvent({
        type: "task_tree.init",
        rootRef: stringField(args, "rootRef"),
        expectedDeliverables: optionalJsonValue(args, "expectedDeliverables"),
        nodes: requiredJsonValue(args, "nodes"),
      });
    },
    mutateSnapshot(args, snapshot) {
      snapshot.rootRef = stringField(args, "rootRef");
      snapshot.expectedDeliverables =
        (args.expectedDeliverables as TreeSnapshot["expectedDeliverables"]) ??
        undefined;
      for (const raw of args.nodes as JsonObject[]) {
        const nodeRef = stringField(raw, "nodeRef");
        snapshot.nodes[nodeRef] = { ...raw };
      }
    },
  });
}

function taskTreeUpsertNodeTool(): ToolInstance {
  return localTool({
    name: "task_tree_upsert_node",
    description:
      "Add or update a single task tree node. Use when splitting work, adding TODOs, or revising descriptions.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        nodeRef: { type: "string" },
        parentRef: { type: ["string", "null"] },
        depth: { type: "number" },
        isLeaf: { type: "boolean" },
        title: { type: "string" },
        description: { type: "string" },
        successCriteria: { type: "string" },
        order: { type: "number" },
        timeout: { type: ["number", "null"] },
      },
      required: [
        "nodeRef",
        "parentRef",
        "depth",
        "isLeaf",
        "title",
        "description",
        "successCriteria",
        "order",
        "timeout",
      ],
    },
    buildEvent(args) {
      return bridgeEvent({
        type: "task_tree.upsert_node",
        nodeRef: stringField(args, "nodeRef"),
        parentRef: nullableStringField(args, "parentRef"),
        depth: numberField(args, "depth"),
        isLeaf: Boolean(args.isLeaf),
        title: stringField(args, "title"),
        description: stringField(args, "description"),
        successCriteria: stringField(args, "successCriteria"),
        order: numberField(args, "order"),
        timeout: optionalJsonValue(args, "timeout"),
      });
    },
    mutateSnapshot(args, snapshot) {
      const nodeRef = stringField(args, "nodeRef");
      snapshot.nodes[nodeRef] = { ...args };
    },
  });
}

function taskTreeUpdateStatusTool(): ToolInstance {
  return localTool({
    name: "task_tree_update_status",
    description:
      "Update a node's status as you work. Transition leaves pending → in_progress → completed/failed.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        nodeRef: { type: "string" },
        status: {
          type: "string",
          enum: [
            "pending",
            "in_progress",
            "completed",
            "failed",
            "timed_out",
            "cancelled",
          ],
        },
        result: {
          description:
            "Optional result payload when completing or failing a node.",
        },
      },
      required: ["nodeRef", "status", "result"],
    },
    buildEvent(args) {
      return bridgeEvent({
        type: "task_tree.update_status",
        nodeRef: stringField(args, "nodeRef"),
        status: stringField(args, "status"),
        result: optionalJsonValue(args, "result"),
      });
    },
    mutateSnapshot(args, snapshot) {
      const nodeRef = stringField(args, "nodeRef");
      const existing = snapshot.nodes[nodeRef] ?? { nodeRef };
      snapshot.nodes[nodeRef] = {
        ...existing,
        status: args.status,
        ...(args.result !== null && args.result !== undefined
          ? { result: args.result }
          : {}),
      };
    },
  });
}

function reportDeliverableTool(): ToolInstance {
  return localTool({
    name: "report_deliverable",
    description:
      "Report a deliverable (URL, file, image, or text) for client delivery. Call when you produce output the client should receive.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        type: {
          type: "string",
          enum: ["text", "image", "file", "url"],
        },
        url: { type: ["string", "null"] },
        content: { type: ["string", "null"] },
        label: { type: ["string", "null"] },
        filename: { type: ["string", "null"] },
        mimeType: { type: ["string", "null"] },
      },
      required: ["type", "url", "content", "label", "filename", "mimeType"],
    },
    buildEvent(args) {
      const deliverable: JsonObject = { type: stringField(args, "type") };
      if (args.url) deliverable.url = args.url;
      if (args.content) deliverable.content = args.content;
      if (args.label) deliverable.label = args.label;
      if (args.filename) deliverable.filename = args.filename;
      if (args.mimeType) deliverable.mimeType = args.mimeType;
      return { type: "deliverable.report", deliverable };
    },
  });
}

function completeTaskTool(): ToolInstance {
  return localTool({
    name: "complete_task",
    description:
      "Signal that the entire task is finished. Call once all work and verification are done.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        summary: {
          type: "string",
          description: "Brief summary of what was accomplished.",
        },
        status: {
          type: "string",
          enum: ["completed", "failed"],
        },
      },
      required: ["summary", "status"],
    },
    buildEvent(args) {
      return {
        type: "task.complete",
        summary: stringField(args, "summary"),
        status: stringField(args, "status") as "completed" | "failed",
      };
    },
  });
}

function localTool(options: {
  name: string;
  description: string;
  parameters: ToolDefinition["parameters"];
  buildEvent: (args: JsonObject) => JsonObject;
  mutateSnapshot?: (args: JsonObject, snapshot: TreeSnapshot) => void;
}): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: options.name,
      description: options.description,
      parameters: options.parameters,
    },
    handler: {
      async execute(args, execution): Promise<ToolResult> {
        try {
          const bridgeEvent = options.buildEvent(args);
          if (options.mutateSnapshot) {
            const snapshot = await loadOrCreateSnapshot(execution);
            options.mutateSnapshot(args, snapshot);
            snapshot.updatedAt = new Date().toISOString();
            await execution.context.exoharness.current.conversation.writeArtifactJson(
              {
                path: TASK_TREE_ARTIFACT_PATH,
                value: snapshot as unknown as JsonValue,
              },
            );
          }
          const payload: BridgeEventPayload = { ok: true, bridgeEvent };
          return payload as unknown as ToolResult;
        } catch (err) {
          return {
            ok: false,
            error: err instanceof Error ? err.message : String(err),
          };
        }
      },
    },
  };
}

async function loadOrCreateSnapshot(execution: {
  context: TurnContext;
}): Promise<TreeSnapshot> {
  const conversation = execution.context.exoharness.current.conversation;
  const latest = latestArtifactVersion(
    await conversation.listArtifacts(),
    TASK_TREE_ARTIFACT_PATH,
  );
  if (!latest) {
    return {
      rootRef: "root",
      nodes: {},
      updatedAt: new Date().toISOString(),
    };
  }
  const existing = await conversation.readArtifactJson<TreeSnapshot>({
    artifactId: latest.artifactId,
    version: latest.version,
  });
  if (existing) return existing;
  return {
    rootRef: "root",
    nodes: {},
    updatedAt: new Date().toISOString(),
  };
}

function latestArtifactVersion(
  artifacts: ArtifactVersion[],
  path: string,
): ArtifactVersion | null {
  return (
    artifacts
      .filter((artifact) => artifact.path === path)
      .sort((a, b) => b.version - a.version)[0] ?? null
  );
}

/** Build a bridge event object without undefined values (JsonObject-safe). */
function bridgeEvent(
  fields: Record<string, JsonValue | undefined>,
): JsonObject {
  const out: JsonObject = {};
  for (const [key, value] of Object.entries(fields)) {
    if (value !== undefined) {
      out[key] = value;
    }
  }
  return out;
}

function optionalJsonValue(
  args: JsonObject,
  key: string,
): JsonValue | undefined {
  if (!(key in args)) return undefined;
  const value = args[key];
  if (value === undefined) return undefined;
  return value as JsonValue;
}

function requiredJsonValue(args: JsonObject, key: string): JsonValue {
  const value = optionalJsonValue(args, key);
  if (value === undefined) {
    throw new Error(`${key} is required`);
  }
  return value;
}

function nodeDraftSchema(): JsonObject {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      nodeRef: { type: "string" },
      parentRef: { type: ["string", "null"] },
      depth: { type: "number" },
      isLeaf: { type: "boolean" },
      title: { type: "string" },
      description: { type: "string" },
      successCriteria: { type: "string" },
      order: { type: "number" },
      timeout: { type: ["number", "null"] },
    },
    required: [
      "nodeRef",
      "parentRef",
      "depth",
      "isLeaf",
      "title",
      "description",
      "successCriteria",
      "order",
      "timeout",
    ],
  };
}

function stringField(args: JsonObject, key: string): string {
  const value = args[key];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${key} must be a non-empty string`);
  }
  return value;
}

function nullableStringField(args: JsonObject, key: string): string | null {
  const value = args[key];
  if (value === null || value === undefined) return null;
  if (typeof value !== "string") {
    throw new Error(`${key} must be a string or null`);
  }
  return value;
}

function numberField(args: JsonObject, key: string): number {
  const value = args[key];
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error(`${key} must be a number`);
  }
  return value;
}

/** Exported for tests — generate a nodeRef when the model omits one. */
export function newNodeRef(prefix: string): string {
  return `${prefix}-${randomUUID().slice(0, 8)}`;
}
