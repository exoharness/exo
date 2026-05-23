import type { JsonObject, ToolDefinition, TurnContext } from "./index";
import type { HarnessToolRegistry, ToolInstance } from "./tools";

export type SchedulerToolName =
  | "schedule_sandbox_task"
  | "list_scheduled_tasks"
  | "cancel_scheduled_task"
  | "delete_scheduled_task";

export function createSchedulerToolInstances(): ToolInstance[] {
  return [
    createScheduleSandboxTaskTool(),
    createListScheduledTasksTool(),
    createCancelScheduledTaskTool(),
    createDeleteScheduledTaskTool(),
  ];
}

export function registerSchedulerTools(
  registry: HarnessToolRegistry,
  names: SchedulerToolName[] = [
    "schedule_sandbox_task",
    "list_scheduled_tasks",
    "cancel_scheduled_task",
    "delete_scheduled_task",
  ],
): void {
  const requested = new Set<SchedulerToolName>(names);
  for (const tool of createSchedulerToolInstances()) {
    if (requested.has(tool.definition.name as SchedulerToolName)) {
      registry.register(tool);
    }
  }
}

function createScheduleSandboxTaskTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "schedule_sandbox_task",
      description:
        "Schedule a recurring command to run in this conversation's sandbox. A host scheduler owns timing and will wake this conversation with compact results when runs complete. The scheduler reuses the shared conversation sandbox when available; use setupCommand for task-specific setup that should run before each scheduled run.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "Stable task name using letters, numbers, dashes, or underscores.",
          },
          schedule: {
            type: "string",
            description:
              "Schedule as '@every 10m', '@every 1h', or a simple cron interval like '*/30 * * * *'.",
          },
          command: {
            type: "array",
            items: { type: "string" },
            minItems: 1,
            description:
              "Command argv to run in the sandbox, for example ['bash', '-lc', 'curl -fsSL https://example.com/health'].",
          },
          sandboxMode: {
            anyOf: [
              { type: "string", enum: ["conversation", "task_fresh"] },
              { type: "null" },
            ],
            description:
              "Sandbox selection mode. Use 'conversation' or null to run in the shared persistent conversation sandbox. Use 'task_fresh' to create a separate fresh sandbox for this task and reuse it across that task's runs.",
          },
          setupCommand: {
            anyOf: [
              {
                type: "array",
                items: { type: "string" },
                minItems: 1,
              },
              { type: "null" },
            ],
            description:
              "Optional argv to run immediately before each scheduled run in the shared conversation sandbox, for example ['bash', '-lc', 'apt-get update && apt-get install -y curl']. Use this for dependencies that should be prepared before each run.",
          },
          reportPrompt: {
            type: "string",
            description:
              "Instructions for how to report each completed run back to the user.",
          },
          maxOutputBytes: {
            type: ["number", "null"],
            description:
              "Maximum bytes to retain from each output stream before truncating, or null for the default.",
          },
        },
        required: [
          "name",
          "schedule",
          "command",
          "sandboxMode",
          "setupCommand",
          "reportPrompt",
          "maxOutputBytes",
        ],
      },
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "schedule_sandbox_task",
          arguments: withConversationScope(execution.context, args),
        });
      },
    },
  };
}

function createListScheduledTasksTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "list_scheduled_tasks",
      description:
        "List scheduled sandbox tasks for this conversation. Disabled tasks are hidden unless includeDisabled is true.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          includeDisabled: {
            type: ["boolean", "null"],
            description:
              "Whether to include disabled/cancelled tasks. Use false or null for the default active-task view.",
          },
        },
        required: ["includeDisabled"],
      },
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "list_scheduled_tasks",
          arguments: withConversationScope(execution.context, args),
        });
      },
    },
  };
}

function createDeleteScheduledTaskTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "delete_scheduled_task",
      description:
        "Permanently delete a scheduled sandbox task for this conversation, including its stored run history. Use cancel_scheduled_task instead when history should be preserved.",
      parameters: taskIdParameters(
        "Scheduled task id returned by schedule_sandbox_task or list_scheduled_tasks.",
      ),
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "delete_scheduled_task",
          arguments: withConversationScope(execution.context, args),
        });
      },
    },
  };
}

function createCancelScheduledTaskTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "cancel_scheduled_task",
      description: "Disable a scheduled sandbox task for this conversation.",
      parameters: taskIdParameters(
        "Scheduled task id returned by schedule_sandbox_task or list_scheduled_tasks.",
      ),
    },
    handler: {
      execute(args, execution) {
        return execution.context.executeTool({
          functionName: "cancel_scheduled_task",
          arguments: withConversationScope(execution.context, args),
        });
      },
    },
  };
}

function taskIdParameters(description: string): ToolDefinition["parameters"] {
  return {
    type: "object",
    additionalProperties: false,
    properties: {
      taskId: {
        type: "string",
        description,
      },
    },
    required: ["taskId"],
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
