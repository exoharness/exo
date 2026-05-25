import { readFileSync } from "node:fs";

import {
  defineHarness,
  registerBuiltInTools,
  registerLibraryToolsFromManifest,
  registerAgentToolsFromManifestPathIfExists,
  registerAdapterTools,
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

const EXOCLAW_IDENTITY_PROMPT = readFileSync(
  new URL("./prompts/me.md", import.meta.url),
  "utf8",
).trim();

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
  registerAdapterTools(tools);
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
      content: EXOCLAW_IDENTITY_PROMPT,
    },
    {
      role: "developer",
      content:
        'This is the Exoclaw long-running agent harness. You can schedule recurring sandbox work with schedule_sandbox_task, inspect active tasks with list_scheduled_tasks, cancel tasks with cancel_scheduled_task, and permanently delete tasks with delete_scheduled_task. You can also create long-running external adapters with create_adapter, inspect them with list_adapters, disable/delete them, and send explicit outbound replies with send_adapter_message. Use cancel_scheduled_task or disable_adapter when history should be preserved; use delete_scheduled_task or delete_adapter when the user asks to remove something entirely. Conversations default to sandboxScope: "agent", so shell commands use this agent\'s shared sandbox unless the conversation was configured with sandboxScope: "conversation". Scheduled tasks default to sandboxMode: "agent". Use sandboxMode: "conversation" when the task should run in this conversation\'s sandbox, and sandboxMode: "task_fresh" when the task should have a separate fresh sandbox that is reused across that task\'s runs. IRC and WhatsApp adapters wake this conversation when their trigger policy matches; do not auto-send model text to external services. Call send_adapter_message only for intentional external replies, using the target value from the inbound wakeup when one is provided. If an adapter message asks you to schedule future work and the future result should appear externally, include the adapterId and target in the scheduled task reportPrompt so the scheduler wakeup can call send_adapter_message.',
    },
  ];
}
