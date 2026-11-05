/**
 * Fail-shut policy gate — the second primitive from exoharness/exo#215.
 * Sits between a model-proposed tool call and its execution: allowed calls
 * pass through untouched, everything else (including evaluator errors) is
 * denied. Fail-shut means the gate can never turn an unknown state into an
 * execution: a policy that cannot be evaluated denies.
 */

export interface ProposedToolCall {
  readonly tool: string;
  readonly args?: unknown;
}

export type PolicyDecision =
  | { readonly verdict: "allow" }
  | { readonly verdict: "deny"; readonly reason: string };

/**
 * A policy maps a proposed tool call to a decision. Any exception thrown by
 * the policy is converted to a denial by the gate — policies are untrusted
 * with respect to liveness, never with respect to safety.
 */
export type ToolPolicy = (call: ProposedToolCall) => PolicyDecision;

export function allow(): PolicyDecision {
  return { verdict: "allow" };
}

export function deny(reason: string): PolicyDecision {
  return { verdict: "deny", reason };
}

/**
 * Allow-list policy: only named tools pass; everything else is denied.
 * Built-in because it is the smallest useful fail-shut default.
 */
export function allowList(tools: readonly string[]): ToolPolicy {
  const set = new Set(tools);
  return (call) =>
    set.has(call.tool)
      ? allow()
      : deny(`tool ${JSON.stringify(call.tool)} is not on the allow list`);
}

/**
 * Compose policies so that the first denial wins — a stricter policy can
 * never be overridden by a looser one downstream.
 */
export function allOf(...policies: readonly ToolPolicy[]): ToolPolicy {
  return (call) => {
    for (const policy of policies) {
      const decision = policy(call);
      if (decision.verdict === "deny") {
        return decision;
      }
    }
    return allow();
  };
}

/**
 * The gate. Evaluates the policy around the proposed call and fails shut:
 * every error path — policy throwing, policy returning a malformed decision,
 * no policy configured — resolves to a denial. Only an explicit allow from a
 * policy that ran to completion permits execution.
 */
export function gate(
  policy: ToolPolicy | null | undefined,
  call: ProposedToolCall,
): PolicyDecision {
  if (typeof policy !== "function") {
    return deny("no policy configured");
  }
  let decision: PolicyDecision;
  try {
    decision = policy(call);
  } catch (error) {
    return deny(
      `policy evaluation failed: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
  if (
    decision === null ||
    typeof decision !== "object" ||
    (decision.verdict !== "allow" && decision.verdict !== "deny")
  ) {
    return deny("policy returned a malformed decision");
  }
  if (decision.verdict === "deny" && !decision.reason) {
    return { verdict: "deny", reason: "policy denied without a reason" };
  }
  return decision;
}

/**
 * Convenience wrapper: run `exec` only when the gate allows; otherwise
 * return the denial. The wrapped executor never learns the gate exists.
 */
export async function governed<T>(
  policy: ToolPolicy | null | undefined,
  call: ProposedToolCall,
  exec: (c: ProposedToolCall) => Promise<T>,
): Promise<
  | { readonly ok: true; readonly value: T }
  | { readonly ok: false; readonly decision: PolicyDecision }
> {
  const decision = gate(policy, call);
  if (decision.verdict === "deny") {
    return { ok: false, decision };
  }
  return { ok: true, value: await exec(call) };
}
