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

export function parseHarborRequest(_value: unknown): HarborRequest {
  // TODO: validate the discriminant and every field. Reject unknown types
  // outright — a typo must not silently become a no-op wakeup.
  throw new Error("not implemented");
}

export function parseHarborResponse(
  _text: string,
  _request: HarborRequest,
): HarborResponse {
  // TODO: parse Exo's send_adapter_message text as JSON and check the type
  // matches expectedResponseType(request) and the trial_id matches. A reply
  // for the wrong trial must never satisfy the current waiter.
  throw new Error("not implemented");
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
  _request: HarborRequest,
  _target: string,
): string {
  // TODO: two shapes.
  //
  // task_started — the task instruction, plus how to declare completion:
  //   reply with send_adapter_message, target <target>, body
  //   {"type":"task_complete","trial_id":...}. Ending a turn is not
  //   completion; grading starts only on that message.
  //
  // verification_result — the reward and verifier output, framed as feedback
  //   on the task just finished. Reply with {"type":"feedback_processed",...}.
  //   Exo needs to know this is a learning signal, or it cannot direct
  //   improvement at anything (docs/eval-design.md, "Self-improvement").
  //   Leave what to do with it to Exo.
  throw new Error("not implemented");
}
