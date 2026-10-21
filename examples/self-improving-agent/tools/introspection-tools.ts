import type { HarnessToolRegistry } from "@exo/harness";

import { registerHostTool } from "./host-tools";

export function registerIntrospectionTools(
  registry: HarnessToolRegistry,
): void {
  registerHostTool(registry, {
    name: "list_conversation_events",
    description:
      'List this conversation\'s canonical event log, newest first. By default returns lifecycle and host events only: conversation_created, conversation_forked, session_started, session_ended, error, sandbox_created, sandbox_started, sandbox_stopped, sandbox_snapshotted, rebuild_and_restart_exo (deferred self-update outcome with updateId, status, exitCode, and reason). Pass explicit kinds to query other event types (e.g. tool_requested, tool_result, messages — these can be very large). Each messages event carries a usage annotation with token counts and cost_usd for that model turn, so this tool is also how to answer questions about spend: query kinds ["messages"] and sum usage.cost_usd. Use this to reconstruct restarts, crashes, session history, and per-conversation cost. Read-only.',
    parameters: {
      type: "object",
      additionalProperties: false,
      properties: {
        kinds: {
          type: ["array", "null"],
          items: { type: "string" },
          description:
            "Event kinds to return. Null for the default lifecycle/host set described above.",
        },
        limit: {
          type: ["number", "null"],
          description:
            "Maximum events to return (default 50, capped at 200). Null for the default.",
        },
        cursor: {
          type: ["string", "null"],
          description:
            "Event id cursor from a previous call's result for pagination. Null to start from the newest (or oldest for asc) event.",
        },
        direction: {
          type: ["string", "null"],
          enum: ["asc", "desc", null],
          description: "Listing order. Null for desc (newest first).",
        },
      },
      required: ["kinds", "limit", "cursor", "direction"],
    },
  });
}
