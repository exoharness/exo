import type {
  ArtifactVersion,
  JsonObject,
  JsonValue,
  TurnContext,
} from "@exo/harness";

export const TASK_TREE_ARTIFACT_PATH = "task-tree.json";

export type TaskTreeSnapshot = {
  rootRef: string;
  expectedDeliverables?: Array<{
    type: string;
    description: string;
    quantity: number;
  }>;
  nodes: Record<string, JsonObject>;
  updatedAt: string;
  /** Set when complete_task succeeds — harness uses this as the turn exit signal. */
  taskComplete?: {
    summary: string;
    status: "completed" | "failed";
    completedAt: string;
  };
};

export function isTaskTreeFinished(
  snapshot: TaskTreeSnapshot | null | undefined,
): boolean {
  return Boolean(snapshot?.taskComplete);
}

export async function readTaskTreeSnapshot(
  context: TurnContext,
): Promise<TaskTreeSnapshot | null> {
  const conversation = context.exoharness.current.conversation;
  const latest = latestArtifactVersion(
    await conversation.listArtifacts(),
    TASK_TREE_ARTIFACT_PATH,
  );
  if (!latest) {
    return null;
  }
  return conversation.readArtifactJson<TaskTreeSnapshot>({
    artifactId: latest.artifactId,
    version: latest.version,
  });
}

export async function loadOrCreateTaskTreeSnapshot(
  context: TurnContext,
): Promise<TaskTreeSnapshot> {
  const existing = await readTaskTreeSnapshot(context);
  if (existing) {
    return existing;
  }
  return {
    rootRef: "root",
    nodes: {},
    updatedAt: new Date().toISOString(),
  };
}

export async function writeTaskTreeSnapshot(
  context: TurnContext,
  snapshot: TaskTreeSnapshot,
): Promise<void> {
  snapshot.updatedAt = new Date().toISOString();
  await context.exoharness.current.conversation.writeArtifactJson({
    path: TASK_TREE_ARTIFACT_PATH,
    value: snapshot as unknown as JsonValue,
  });
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

export const DEFAULT_ROUND_BUDGET_EXTENSIONS = 3;

export function buildRoundBudgetContinueMessage(
  extension: number,
  maxExtensions: number,
): string {
  return [
    "Tool round budget reached but the task is not finished yet.",
    "",
    "Keep going — use tools to unblock yourself (e2b_run_command, executeCommand, createPresentation, etc.).",
    "Do not call complete_task with status failed for recoverable tooling errors.",
    "Only call complete_task when the deliverable is ready (status completed) or you are truly blocked after trying alternatives.",
    "",
    `(Budget extension ${extension}/${maxExtensions} — continue with the next tool call.)`,
  ].join("\n");
}

export function buildAutonomousContinueUserMessage(
  lastAssistantText?: string,
): string {
  const tail = lastAssistantText?.trim().slice(0, 400);
  const lines = [
    "Continue working on this task. You have not called complete_task yet.",
    "",
    "Use tools in this turn — do not reply with text-only plans.",
    "Recover from errors with e2b_run_command or executeCommand; do not call complete_task failed for fixable tooling issues.",
    "When every TODO leaf is done and deliverables are reported, call complete_task.",
    'If you are blocked after exhausting alternatives, call complete_task with status "failed" and explain why.',
  ];
  if (tail) {
    lines.push("", "Your last message was:", tail);
  }
  return lines.join("\n");
}
