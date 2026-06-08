import { existsSync, readFileSync } from "node:fs";

import {
  defineHarness,
  registerBuiltInTools,
  registerAgentToolsFromDirectoryIfExists,
  registerLibraryToolModulePath,
  registerAdapterTools,
  type BuiltInToolName,
  type HarnessToolRegistry,
  type Message,
  type TurnContext,
} from "@exo/harness";

import { registerSchedulerTools } from "./scheduler-tools";
import { registerSandboxTools } from "./sandbox-tools";
import { registerGuardianTools } from "./guardian-tools";
import {
  basicHarnessInstructions,
  defaultBuiltInToolNames,
  runResponsesHarnessTurn,
} from "../typescript/turn-loop";

const EXOCLAW_IDENTITY_PROMPT = readFileSync(
  new URL("./prompts/me.md", import.meta.url),
  "utf8",
).trim();
const DEFAULT_LOCAL_PROMPT_PATH = ".exo/exoclaw-profile.md";
const DEFAULT_EXOCLAW_REPO = "/workspace/exo";
const DEFAULT_EXOCLAW_SELF_MAP = `${DEFAULT_EXOCLAW_REPO}/examples/exoclaw/SELF.md`;

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
  registerSandboxTools(tools);
  registerGuardianTools(tools);
  for (const modulePath of context.agentConfig.typescript?.toolModulePaths ??
    []) {
    await registerLibraryToolModulePath(tools, context, modulePath);
  }
  if (context.agentConfig.enableAgentToolCreation) {
    await registerAgentToolsFromDirectoryIfExists(tools, context);
  }
}

function builtInToolNames(context: TurnContext): BuiltInToolName[] {
  return defaultBuiltInToolNames(context);
}

function exoclawInstructions(context: TurnContext): Message[] {
  const repoPath = process.env.EXOCLAW_REPO ?? DEFAULT_EXOCLAW_REPO;
  const selfMapPath = process.env.EXOCLAW_SELF_MAP ?? DEFAULT_EXOCLAW_SELF_MAP;
  const instructions: Message[] = [
    ...basicHarnessInstructions(context),
    {
      role: "developer",
      content: EXOCLAW_IDENTITY_PROMPT,
    },
    {
      role: "developer",
      content:
        'This is the Exoclaw long-running agent harness. You can schedule recurring sandbox work with schedule_sandbox_task, inspect active tasks with list_scheduled_tasks, cancel tasks with cancel_scheduled_task, and permanently delete tasks with delete_scheduled_task. You can inspect sandbox filesystem snapshots with list_sandbox_snapshots, capture a checkpoint with snapshot_sandbox, and rewind to a previous checkpoint with rewind_sandbox. You can use guardian_action for host-side self-maintenance such as checking service status, building Exoclaw, viewing logs, and restarting the scheduler or adapter runners while preserving .exo state. guardian_action restart actions are deferred briefly so the current turn can finish before services stop; guardian builds also ask the control REPL wrapper to refresh its child process without closing the user\'s terminal. After requesting one, report that it was scheduled and use status/logs after services come back. You can also create long-running external adapters with create_adapter, inspect them with list_adapters, disable/delete them, and send explicit outbound replies with send_adapter_message. Use cancel_scheduled_task or disable_adapter when history should be preserved; use delete_scheduled_task or delete_adapter when the user asks to remove something entirely. Conversations default to sandboxScope: "agent", so shell commands use this agent\'s shared sandbox unless the conversation was configured with sandboxScope: "conversation". Scheduled tasks default to sandboxMode: "agent". Use sandboxMode: "conversation" when the task should run in this conversation\'s sandbox, and sandboxMode: "task_fresh" when the task should have a separate fresh sandbox that is reused across that task\'s runs. IRC, WhatsApp, Signal, and Discord adapters wake this conversation when their trigger policy matches; do not auto-send model text to external services. Call send_adapter_message only for intentional external replies, using the target value from the inbound wakeup when one is provided. For Discord, the target is a channel id unless the adapter has a defaultChannelId. If an adapter message asks you to schedule future work and the future result should appear externally, include the adapterId and target in the scheduled task reportPrompt so the scheduler wakeup can call send_adapter_message.',
    },
    {
      role: "developer",
      content: `Your own source tree is mounted in the sandbox at ${repoPath}. Start with ${selfMapPath} when you need to inspect or modify Exoclaw itself. Use guardian_action for host-side builds and service restarts after code changes.`,
    },
  ];
  const localPrompt = readLocalPrompt();
  if (localPrompt !== null) {
    instructions.push({
      role: "developer",
      content: localPrompt,
    });
  }
  return instructions;
}

function readLocalPrompt(): string | null {
  const path =
    process.env.EXOCLAW_LOCAL_PROMPT_FILE ?? DEFAULT_LOCAL_PROMPT_PATH;
  if (!existsSync(path)) {
    return null;
  }
  const contents = readFileSync(path, "utf8").trim();
  return contents.length === 0 ? null : contents;
}
