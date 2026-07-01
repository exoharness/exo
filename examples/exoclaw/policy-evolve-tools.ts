import { randomUUID } from "node:crypto";

import type {
  Agent,
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

// evolve_policy: the agent's request to verify-and-adopt a self-change.
//
// The agent edits its own code in the policy sandbox (policy_shell), then calls
// this. It records an evolve request as an agent artifact (the same durable
// artifact mechanism memory-tools uses) so the host-side policy supervisor --
// which lives OUTSIDE the policy sandbox -- can pick it up, health-check the new
// code, and roll back to the last good snapshot on failure. The agent can't do
// that itself: if the change breaks its own executor, it can't verify or rewind
// the container it runs in.
//
// This mirrors the old exoclaw marker pattern (guardian wrote .exo/*.restart;
// the control wrapper polled and claimed it) -- except the marker is an EH
// artifact, because the executor (in the sandbox) and the supervisor (on the
// host) no longer share a filesystem, only the kernel.
const EVOLVE_REQUEST_ARTIFACT_PATH = "policy/evolve-request.json";

type EvolveHandle = Pick<Agent, "writeArtifactJson">;

function evolvePolicyTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "evolve_policy",
      description:
        "Request that your just-edited policy code be verified and switched to, with automatic rollback if it fails to run. First edit your code in the policy sandbox with policy_shell, then call this. A supervisor outside your sandbox health-checks the new code: if it fails, it rewinds to the last known-good snapshot, discarding this edit; if it passes, it snapshots the new state as the new baseline. (That known-good baseline is the last accepted version, captured before this edit — not taken now, since your code is already changed.) Set rebuild=true only if you changed Rust (crates/) so the executor binary must be recompiled; use false for TypeScript-only policy edits (prompts, harness.ts, tools).",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          rebuild: {
            type: "boolean",
            description:
              "Whether the executor binary must be recompiled (Rust/crates changes). false for TypeScript-only edits.",
          },
          note: {
            type: "string",
            description:
              "Short description of the change, recorded in the evolution log.",
          },
        },
        required: ["rebuild", "note"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const note = typeof args.note === "string" ? args.note.trim() : "";
        if (note.length === 0) {
          return { ok: false, error: "note is required" };
        }
        const request = {
          rebuild: args.rebuild === true,
          note,
          requestedAt: new Date().toISOString(),
          nonce: `evo_${randomUUID().slice(0, 8)}`,
        };
        const handle = evolveHandle(execution.context);
        await handle.writeArtifactJson({
          path: EVOLVE_REQUEST_ARTIFACT_PATH,
          value: request as unknown as JsonValue,
        });
        return { ok: true, scheduled: true, nonce: request.nonce };
      },
    },
  };
}

function evolveHandle(context: TurnContext): EvolveHandle {
  return context.exoharness.current.agent;
}

export function registerEvolveTools(registry: HarnessToolRegistry): void {
  registry.register(evolvePolicyTool());
}
