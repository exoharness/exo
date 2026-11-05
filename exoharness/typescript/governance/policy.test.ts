import { describe, expect, it, vi } from "vitest";
import {
  allOf,
  allow,
  allowList,
  deny,
  gate,
  governed,
  type ProposedToolCall,
} from "./policy";

const bash: ProposedToolCall = { tool: "bash", args: { cmd: "ls" } };

describe("gate", () => {
  it("passes allowed calls through untouched", () => {
    expect(gate(allowList(["bash", "read"]), bash)).toEqual({
      verdict: "allow",
    });
  });

  it("denies tools not on the allow list", () => {
    const decision = gate(allowList(["read"]), bash);
    expect(decision.verdict).toBe("deny");
  });

  it("fails shut when no policy is configured", () => {
    expect(gate(null, bash)).toEqual({
      verdict: "deny",
      reason: "no policy configured",
    });
  });

  it("fails shut when the policy throws", () => {
    const policy = vi.fn(() => {
      throw new Error("policy backend unreachable");
    });
    expect(gate(policy, bash)).toEqual({
      verdict: "deny",
      reason: "policy evaluation failed: policy backend unreachable",
    });
  });

  it("fails shut on a malformed decision", () => {
    // eslint-disable-next-line -- deliberately malformed
    const malformed = { verdict: "ALLOW" } as never;
    const policy = vi.fn(() => malformed);
    expect(gate(policy, bash).verdict).toBe("deny");
  });

  it("normalizes a reason-less denial", () => {
    const decision = gate(() => ({ verdict: "deny" }) as never, bash);
    expect(decision).toEqual({
      verdict: "deny",
      reason: "policy denied without a reason",
    });
  });
});

describe("allOf", () => {
  it("first denial wins", () => {
    const policy = allOf(allowList(["bash"]), () =>
      deny("second opinion says no"),
    );
    expect(gate(policy, bash).verdict).toBe("deny");
  });

  it("allows when every policy allows", () => {
    const policy = allOf(allowList(["bash"]), () => allow());
    expect(gate(policy, bash)).toEqual({ verdict: "allow" });
  });
});

describe("governed", () => {
  it("executes only on allow", async () => {
    const exec = vi.fn(async () => "ran");
    const allowed = await governed(allowList(["bash"]), bash, exec);
    expect(allowed).toEqual({ ok: true, value: "ran" });
    expect(exec).toHaveBeenCalledOnce();

    const blocked = await governed(allowList(["read"]), bash, exec);
    expect(blocked.ok).toBe(false);
    if (!blocked.ok) {
      expect(blocked.decision.verdict).toBe("deny");
    }
    expect(exec).toHaveBeenCalledOnce();
  });
});
