import type { HarnessToolRegistry } from "@exo/harness";

import { registerHostTool } from "./host-tools";

// policy_shell runs a command in the agent's dedicated *policy* sandbox — its
// own source/code box — distinct from the env sandbox used by `shell`. This is
// the self-edit surface: inspect, edit, and build Exoclaw's own code here. The
// definition delegates to the Rust runtime's `policy_shell` arm, which resolves
// the per-agent policy sandbox (config/policy-sandbox.json) and runs the command
// there. Keeping this separate from `shell` keeps the self-modification surface
// explicit and opt-in.
export function registerPolicyTools(registry: HarnessToolRegistry): void {
  registerHostTool(registry, {
    name: "policy_shell",
    description:
      "Run a shell command in this agent's dedicated policy sandbox — its own source/code box, separate from the env sandbox that `shell` uses. Use this to inspect, edit, and build Exoclaw's own code (for example `cd /workspace/exo && cargo build -p exo`). Do not use it for normal task work; use `shell` for that. State (cwd, installed packages) is independent from the `shell` sandbox.",
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        command: {
          type: "string",
          description: "Shell command to execute in the policy sandbox.",
        },
      },
      required: ["command"],
    },
  });
}
