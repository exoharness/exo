import {
  defineHarness,
  registerBuiltInTools,
  registerLibraryToolsFromManifest,
  registerAgentToolsFromManifestPathIfExists,
  registerSchedulerTools,
  type BuiltInToolName,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import {
  basicHarnessInstructions,
  defaultBuiltInToolNames,
  runResponsesHarnessTurn,
} from "../typescript/turn-loop";

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: exoclawInstructions,
      registerTools: registerExoclawTools,
    });
  },
});

async function registerExoclawTools(
  tools: HarnessToolRegistry,
  context: TurnContext,
): Promise<void> {
  registerBuiltInTools(tools, context, builtInToolNames(context));
  registerSchedulerTools(tools);
  await registerLibraryToolsFromManifest(tools, context, {
    tools: context.agentConfig.libraryTools,
  });
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromManifestPathIfExists(tools, context);
  }
}

function builtInToolNames(context: TurnContext): BuiltInToolName[] {
  return defaultBuiltInToolNames(context);
}

function exoclawInstructions(context: TurnContext): Message[] {
  return [
    ...basicHarnessInstructions(context),
    {
      role: "developer",
      content:
        'This is the Exoclaw long-running agent harness. You can schedule recurring sandbox work with schedule_sandbox_task, inspect active tasks with list_scheduled_tasks, cancel tasks with cancel_scheduled_task, and permanently delete tasks with delete_scheduled_task. Use cancel_scheduled_task when task history should be preserved; use delete_scheduled_task when the user asks to remove a task entirely. Conversations default to sandboxScope: "agent", so shell commands use this agent\'s shared sandbox unless the conversation was configured with sandboxScope: "conversation". Scheduled tasks default to sandboxMode: "agent". Use sandboxMode: "conversation" when the task should run in this conversation\'s sandbox, and sandboxMode: "task_fresh" when the task should have a separate fresh sandbox that is reused across that task\'s runs.',
    },
  ];
}
