import os from "node:os";
import path from "node:path";

// Pure helpers for the Harbor adapter worker. The worker listens on a local
// unix socket; the host-side Harbor integration (eval/harbor) sends one
// request per exchange and blocks for the matching response.
//
// Two exchanges per trial:
//   task_started        -> task_complete
//   verification_result -> feedback_processed

export type HarborTaskStarted = {
  type: "task_started";
  message_id: string;
  trial_id: string;
  task_name: string;
  instruction: string;
  conversation_id: string;
  sandbox_id: string;
  step_name?: string | null;
  deadline_at?: string | null;
};

export type HarborVerificationResult = {
  type: "verification_result";
  message_id: string;
  trial_id: string;
  task_name: string;
  conversation_id: string;
  rewards?: Record<string, number> | null;
  verifier_stdout?: string | null;
  verifier_stderr?: string | null;
  exception?: {
    type: string;
    message: string;
    traceback?: string | null;
  } | null;
};

export type HarborRequest = HarborTaskStarted | HarborVerificationResult;

export type HarborResponse =
  | { type: "task_complete"; trial_id: string; summary?: string | null }
  | { type: "feedback_processed"; trial_id: string; summary?: string | null };

export function defaultSocketPath(homedir: string = os.homedir()): string {
  return path.join(homedir, ".exo", "harbor.sock");
}

/// Correlation key for one exchange. Exo echoes this back as the
/// `send_adapter_message` target, which is how a reply finds its waiter.
/// Keyed by request type as well as trial so the two exchanges within a trial
/// cannot satisfy each other.
export function requestTarget(request: HarborRequest): string {
  return `harbor:${request.trial_id}:${request.type}`;
}

export function expectedResponseType(
  request: HarborRequest,
): HarborResponse["type"] {
  return request.type === "task_started"
    ? "task_complete"
    : "feedback_processed";
}

export function parseHarborRequest(value: unknown): HarborRequest {
  const record = objectValue(value, "Harbor request");
  const type = stringValue(record.type, "request type");
  const base = {
    message_id: stringValue(record.message_id, "message_id"),
    trial_id: stringValue(record.trial_id, "trial_id"),
    task_name: stringValue(record.task_name, "task_name"),
    conversation_id: stringValue(record.conversation_id, "conversation_id"),
  };

  if (type === "task_started") {
    return {
      type,
      ...base,
      instruction: stringValue(record.instruction, "instruction"),
      sandbox_id: stringValue(record.sandbox_id, "sandbox_id"),
      step_name: nullableString(record.step_name, "step_name"),
      deadline_at: nullableString(record.deadline_at, "deadline_at"),
    };
  }
  if (type === "verification_result") {
    return {
      type,
      ...base,
      rewards: nullableNumberMap(record.rewards),
      verifier_stdout: nullableString(
        record.verifier_stdout,
        "verifier_stdout",
      ),
      verifier_stderr: nullableString(
        record.verifier_stderr,
        "verifier_stderr",
      ),
      exception: nullableException(record.exception),
    };
  }
  // Unknown types are rejected rather than ignored: a typo must surface as an
  // error on the host side, not as a wakeup that silently never arrives.
  throw new Error("request type must be task_started or verification_result");
}

export function parseHarborResponse(
  text: string,
  request: HarborRequest,
): HarborResponse {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    throw new Error("Harbor response must be a JSON object");
  }
  const record = objectValue(value, "Harbor response");
  const type = stringValue(record.type, "response type");
  const expected = expectedResponseType(request);
  if (type !== expected) {
    throw new Error(`response type must be ${expected}`);
  }
  const trialId = stringValue(record.trial_id, "trial_id");
  if (trialId !== request.trial_id) {
    // A reply carrying another trial's id must never satisfy this waiter.
    throw new Error(
      `response trial_id ${trialId} does not match request ${request.trial_id}`,
    );
  }
  return {
    type,
    trial_id: trialId,
    summary: nullableString(record.summary, "summary"),
  } as HarborResponse;
}

/// Builds the wakeup text Exo sees.
///
/// Say only what Exo cannot work out for itself. Two things qualify: the
/// completion protocol, which is pure convention and unguessable, and the fact
/// that the verification message is feedback to learn from rather than a new
/// task.
///
/// Keep this minimal for a second reason: every instruction added here is
/// prompt engineering that shapes results, and the eval is supposed to measure
/// what Exo improves on its own.
export function composeHarborPrompt(
  request: HarborRequest,
  adapterId: string,
  target: string,
): string {
  // Wording matters more than it should here. An earlier iteration used a
  // "Use exactly:" block with a `<one line>` placeholder inside the JSON —
  // self-contradictory, and models responded by printing the call as prose
  // instead of making it. This phrasing is the one with a track record.
  //
  // adapterId is required by the tool. The runtime appends its own reply
  // instruction carrying the same ids, but say it here too: this is the only
  // place that can explain the JSON body.
  const replyLine = (type: string) => {
    const example = JSON.stringify({
      type,
      trial_id: request.trial_id,
      summary: "optional short summary",
    });
    return (
      `When you have finished this phase, call send_adapter_message exactly ` +
      `once with adapterId \`${adapterId}\`, target \`${target}\`, and text ` +
      `containing a JSON object shaped like \`${example}\`. An ordinary ` +
      `assistant response does not finish the Harbor phase.`
    );
  };

  if (request.type === "task_started") {
    return [
      `Harbor started trial \`${request.trial_id}\` for task ` +
        `\`${request.task_name}\`. Your shell tool is attached to Exoharness ` +
        `sandbox \`${request.sandbox_id}\`, which is the running Harbor task ` +
        `container.`,
      "",
      "Task instruction:",
      request.instruction,
      "",
      // Ending a turn is not completion. That distinction matters most when
      // Exo restarts itself mid-task: the turn ends, the guardian reboot
      // notice wakes this conversation again, and work continues. Only the
      // explicit message below says the task is ready to be graded.
      replyLine("task_complete"),
    ].join("\n");
  }

  const rewards = request.rewards
    ? JSON.stringify(request.rewards)
    : "(none recorded)";
  return [
    `Evaluation feedback for task "${request.task_name}".`,
    "",
    `Reward: ${rewards}`,
    request.exception
      ? `The trial failed infrastructurally rather than scoring: ` +
        `${request.exception.type}: ${request.exception.message}`
      : "",
    section("Verifier stdout", request.verifier_stdout),
    section("Verifier stderr", request.verifier_stderr),
    "---",
    "The task environment is gone; your shell is back on your own sandbox.",
    "This is feedback on the work you just finished, not a new task.",
    "",
    replyLine("feedback_processed"),
  ]
    .filter((line) => line !== "")
    .join("\n");
}

function section(title: string, body: string | null | undefined): string {
  return body && body.trim().length > 0 ? `\n${title}:\n${body}` : "";
}

type JsonObject = Record<string, unknown>;

function objectValue(value: unknown, name: string): JsonObject {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`${name} must be a JSON object`);
  }
  return value as JsonObject;
}

function stringValue(value: unknown, name: string): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`${name} must be a non-empty string`);
  }
  return value;
}

function nullableString(value: unknown, name: string): string | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string") {
    throw new Error(`${name} must be null or a string`);
  }
  return value;
}

function nullableNumberMap(value: unknown): Record<string, number> | null {
  if (value === undefined || value === null) {
    return null;
  }
  const record = objectValue(value, "rewards");
  const rewards: Record<string, number> = {};
  for (const [key, entry] of Object.entries(record)) {
    if (typeof entry !== "number") {
      throw new Error(`reward ${key} must be a number`);
    }
    rewards[key] = entry;
  }
  return rewards;
}

function nullableException(
  value: unknown,
): HarborVerificationResult["exception"] {
  if (value === undefined || value === null) {
    return null;
  }
  const record = objectValue(value, "exception");
  return {
    type: stringValue(record.type, "exception type"),
    message: stringValue(record.message, "exception message"),
    traceback: nullableString(record.traceback, "exception traceback"),
  };
}
