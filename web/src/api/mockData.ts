import {
  LARGE_CONVERSATION_ID,
  LARGE_PERF_EVENTS,
} from "./mockLargeConversation";
import type {
  AgentRecord,
  Artifact,
  BindingRecord,
  ConversationHandleInfo,
  Event,
  SecretMetadata,
} from "./protocol";

function textToBytes(text: string): number[] {
  return [...new TextEncoder().encode(text)];
}

function ts(minutesAgo: number): string {
  const date = new Date("2026-06-18T14:00:00.000Z");
  date.setMinutes(date.getMinutes() - minutesAgo);
  return date.toISOString();
}

// --- Agents -----------------------------------------------------------------

export const MOCK_AGENTS: AgentRecord[] = [
  {
    id: "agent_01H8RESEARCH0001",
    slug: "research",
    name: "Research Assistant",
  },
  {
    id: "agent_01H8OPS000000001",
    slug: "ops",
    name: "Ops Monitor",
  },
];

// --- Secrets (metadata only) ------------------------------------------------

export const MOCK_ROOT_SECRETS: SecretMetadata[] = [
  {
    id: "sec_root_openai",
    type: "key",
    name: "OPENAI_API_KEY",
    created_at: ts(60 * 24 * 14),
  },
  {
    id: "sec_root_github",
    type: "oauth",
    name: "GITHUB_TOKEN",
    created_at: ts(60 * 24 * 30),
  },
];

export const MOCK_AGENT_SECRETS: Record<string, SecretMetadata[]> = {
  agent_01H8RESEARCH0001: [
    ...MOCK_ROOT_SECRETS,
    {
      id: "sec_agent_brave",
      type: "key",
      name: "BRAVE_SEARCH_API_KEY",
      created_at: ts(60 * 24 * 7),
    },
  ],
  agent_01H8OPS000000001: [
    ...MOCK_ROOT_SECRETS,
    {
      id: "sec_agent_datadog",
      type: "key",
      name: "DATADOG_API_KEY",
      created_at: ts(60 * 24 * 3),
    },
  ],
};

export const MOCK_CONVERSATION_SECRETS: Record<string, SecretMetadata[]> = {
  conv_demo_rich: [
    ...MOCK_AGENT_SECRETS.agent_01H8RESEARCH0001,
    {
      id: "sec_conv_notion",
      type: "oauth",
      name: "NOTION_INTEGRATION",
      created_at: ts(45),
    },
  ],
  conv_quick_check: MOCK_AGENT_SECRETS.agent_01H8RESEARCH0001,
  conv_refactor: MOCK_AGENT_SECRETS.agent_01H8RESEARCH0001,
  conv_deploy: MOCK_AGENT_SECRETS.agent_01H8OPS000000001,
  [LARGE_CONVERSATION_ID]: MOCK_AGENT_SECRETS.agent_01H8RESEARCH0001,
};

// --- Bindings ---------------------------------------------------------------

export const MOCK_ROOT_BINDINGS: BindingRecord[] = [
  {
    id: "bind_root_llm",
    type: "llm",
    name: "default-llm",
    created_at: ts(60 * 24 * 20),
    binding: {
      type: "llm",
      name: "default-llm",
      model: "claude-sonnet-4-20250514",
      base_url: null,
      secret_id: "sec_root_openai",
    },
  },
];

export const MOCK_AGENT_BINDINGS: Record<string, BindingRecord[]> = {
  agent_01H8RESEARCH0001: [
    ...MOCK_ROOT_BINDINGS,
    {
      id: "bind_agent_mcp",
      type: "mcp",
      name: "context7",
      created_at: ts(60 * 24 * 5),
      binding: {
        type: "mcp",
        name: "context7",
        server_url: "https://mcp.context7.com/mcp",
        secret_id: null,
      },
    },
    {
      id: "bind_agent_sandbox",
      type: "sandbox",
      name: "dev-sandbox",
      created_at: ts(60 * 24 * 2),
      binding: {
        type: "sandbox",
        name: "dev-sandbox",
        config: {
          provider: "docker",
          default_image: "node:22-bookworm",
        },
      },
    },
  ],
  agent_01H8OPS000000001: [
    ...MOCK_ROOT_BINDINGS,
    {
      id: "bind_ops_env",
      type: "env",
      name: "DEPLOY_TARGET",
      created_at: ts(60 * 24 * 1),
      binding: {
        type: "env",
        name: "DEPLOY_TARGET",
        env_var: "DEPLOY_TARGET",
        secret_id: "sec_agent_datadog",
      },
    },
  ],
};

export const MOCK_CONVERSATION_BINDINGS: Record<string, BindingRecord[]> = {
  conv_demo_rich: MOCK_AGENT_BINDINGS.agent_01H8RESEARCH0001,
  conv_quick_check: MOCK_AGENT_BINDINGS.agent_01H8RESEARCH0001,
  conv_refactor: MOCK_AGENT_BINDINGS.agent_01H8RESEARCH0001,
  conv_deploy: MOCK_AGENT_BINDINGS.agent_01H8OPS000000001,
  [LARGE_CONVERSATION_ID]: MOCK_AGENT_BINDINGS.agent_01H8RESEARCH0001,
};

// --- Artifacts --------------------------------------------------------------

const DEMO_REPORT_MARKDOWN = `# Session Report

Generated during the protocol demo conversation.

## Summary

- Tool calls completed successfully
- Artifact written to \`reports/session-summary.md\`
- Sandbox process exited with code 0
`;

export const MOCK_ARTIFACTS: Record<string, Artifact> = {
  art_report_v1: {
    artifact_id: "art_report_v1",
    path: "reports/session-summary.md",
    version: 1,
    created_at: ts(12),
    size_bytes: textToBytes(DEMO_REPORT_MARKDOWN).length,
    contents: textToBytes(DEMO_REPORT_MARKDOWN),
  },
};

// --- Conversations ----------------------------------------------------------

const SESSION_DEMO = "sess_demo_001";
const TURN_DEMO_1 = "turn_demo_001";
const TURN_DEMO_2 = "turn_demo_002";
const TOOL_CALL_READ = "tcall_read_file_01";
const RESPONSE_1 = "resp_0001";
const RESPONSE_2 = "resp_0002";
const SANDBOX_ID = "sbox_demo_docker_01";
const PROCESS_ID = "proc_npm_test_01";
const SNAPSHOT_ID = "snap_base_01";

const ASSISTANT_THINKING_TEXT = `<think>
The user wants a walkthrough of the exo event protocol. I should read the
existing transcript fixture first, then summarize the render paths the web UI
covers: messages, tools, artifacts, and sandbox activity.
</think>

I'll inspect the fixture file and summarize what the UI will render.`;

const ASSISTANT_RICH_MARKDOWN = `Here is a structured summary of the **protocol demo** coverage.

## Latency breakdown

| Step | Latency | Status |
|------|---------|--------|
| list agents | 18ms | ok |
| load events | 42ms | ok |
| render markdown | 8ms | ok |

Throughput scales as $T = \\frac{n}{\\Delta t}$ for $n$ events over window $\\Delta t$.

\`\`\`typescript
export function paginateEvents<T extends { id: string }>(
  items: T[],
  cursor: string | null,
  limit: number,
): T[] {
  const start = cursor
    ? items.findIndex((item) => item.id > cursor) + 1
    : 0;
  return items.slice(Math.max(0, start), start + limit);
}
\`\`\`

\`\`\`mermaid
flowchart LR
  subgraph client["exo-web"]
    A[MockClient] --> B[getEventsPage]
    B --> C[Transcript]
  end
  C --> D[Markdown + Tools + Artifacts]
\`\`\`

All render paths above should appear in screenshots without a live backend.`;

function buildDemoRichEvents(conversationId: string): Event[] {
  const e = (
    n: number,
    data: Event["data"],
    extra?: Partial<Event>,
  ): Event => ({
    id: `evt_demo_${String(n).padStart(4, "0")}`,
    conversation_id: conversationId,
    session_id: extra?.session_id ?? null,
    turn_id: extra?.turn_id ?? null,
    created_at: ts(120 - n),
    data,
  });

  return [
    e(1, {
      type: "conversation_created",
      slug: "protocol-demo",
      name: "Protocol Demo",
    }),
    e(2, { type: "session_started" }, { session_id: SESSION_DEMO }),
    e(
      3,
      { type: "turn_started" },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_1 },
    ),
    e(
      4,
      {
        type: "messages",
        messages: [
          {
            role: "user",
            content:
              "Walk me through the exo-web render paths using realistic fixture data — messages, thinking, tools, artifacts, and sandbox output.",
          },
        ],
        response_id: null,
        usage: null,
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_1 },
    ),
    e(
      5,
      { type: "turn_ended" },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_1 },
    ),
    e(
      6,
      { type: "turn_started" },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      7,
      {
        type: "messages",
        messages: [
          {
            role: "assistant",
            content: ASSISTANT_THINKING_TEXT,
            id: "msg_asst_thinking",
          },
        ],
        response_id: RESPONSE_1,
        usage: {
          model: "claude-sonnet-4-20250514",
          prompt_tokens: 842,
          completion_tokens: 156,
          completion_reasoning_tokens: 89,
          duration_ms: 2340,
          ttft_ms: 410,
          cost_usd: 0.004821,
        },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      8,
      {
        type: "tool_requested",
        tool_call_id: TOOL_CALL_READ,
        response_id: RESPONSE_1,
        request: {
          function_name: "read_file",
          arguments: {
            path: "fixtures/protocol-demo.json",
            offset: 0,
            limit: 4096,
          },
        },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      9,
      {
        type: "tool_result",
        tool_call_id: TOOL_CALL_READ,
        result: {
          path: "fixtures/protocol-demo.json",
          bytes: 2048,
          duration_ms: 127,
          excerpt: '{"agents":2,"conversations":4,"events":24}',
        },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      10,
      {
        type: "messages",
        messages: [
          {
            role: "assistant",
            content: ASSISTANT_RICH_MARKDOWN,
            id: "msg_asst_rich_md",
          },
        ],
        response_id: RESPONSE_2,
        usage: {
          model: "claude-sonnet-4-20250514",
          prompt_tokens: 1204,
          completion_tokens: 512,
          duration_ms: 4810,
          ttft_ms: 520,
          cost_usd: 0.009104,
        },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      11,
      {
        type: "artifact_written",
        artifact_id: "art_report_v1",
        path: "reports/session-summary.md",
        version: 1,
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      12,
      {
        type: "sandbox_created",
        sandbox_id: SANDBOX_ID,
        name: "demo-runner",
        provider: "docker",
        image: "node:22-bookworm",
        default_workdir: "/workspace",
        file_system_mounts: [
          {
            host_path: "/Users/reviewer/wt-demo",
            mount_path: "/workspace",
            mode: "rw",
            internal: false,
          },
        ],
        enable_networking: true,
        idle_seconds: 600,
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      13,
      {
        type: "sandbox_started",
        sandbox_id: SANDBOX_ID,
        snapshot_id: SNAPSHOT_ID,
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      14,
      {
        type: "sandbox_process_started",
        sandbox_id: SANDBOX_ID,
        process_id: PROCESS_ID,
        name: "npm-test",
        command: ["npm", "run", "typecheck"],
        cwd: "/workspace/web",
        mode: "exec",
        stdin: "none",
        output: "buffered",
        lifecycle: "detached",
        status: { type: "running" },
        provider_state: { container: "exo-demo-runner" },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      15,
      {
        type: "sandbox_process_event",
        sandbox_id: SANDBOX_ID,
        process_id: PROCESS_ID,
        event: {
          type: "stdout",
          cursor: 1,
          data: textToBytes("> exo-web@0.1.0 typecheck\n> tsc --noEmit\n"),
        },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      16,
      {
        type: "sandbox_process_event",
        sandbox_id: SANDBOX_ID,
        process_id: PROCESS_ID,
        event: {
          type: "exit",
          cursor: 2,
          exit_code: 0,
        },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      17,
      {
        type: "sandbox_process_state_updated",
        sandbox_id: SANDBOX_ID,
        process_id: PROCESS_ID,
        status: { type: "exited", exit_code: 0 },
        provider_state: { container: "exo-demo-runner", finished: true },
      },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(
      18,
      { type: "turn_ended" },
      { session_id: SESSION_DEMO, turn_id: TURN_DEMO_2 },
    ),
    e(19, { type: "session_ended" }, { session_id: SESSION_DEMO }),
  ];
}

function buildQuickCheckEvents(conversationId: string): Event[] {
  return [
    {
      id: "evt_quick_0001",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: "turn_quick_01",
      created_at: ts(30),
      data: {
        type: "conversation_created",
        slug: "quick-check",
        name: "Quick Health Check",
      },
    },
    {
      id: "evt_quick_0002",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: "turn_quick_01",
      created_at: ts(29),
      data: { type: "session_started" },
    },
    {
      id: "evt_quick_0003",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: "turn_quick_01",
      created_at: ts(28),
      data: { type: "turn_started" },
    },
    {
      id: "evt_quick_0004",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: "turn_quick_01",
      created_at: ts(27),
      data: {
        type: "messages",
        messages: [
          { role: "user", content: "Is the mock client wired correctly?" },
        ],
        response_id: null,
        usage: null,
      },
    },
    {
      id: "evt_quick_0005",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: "turn_quick_01",
      created_at: ts(26),
      data: {
        type: "messages",
        messages: [
          {
            role: "assistant",
            content:
              "Yes — swap `ExoClient` for `MockClient` in `App.tsx` and the inspector loads entirely from fixtures.",
            id: "msg_quick_reply",
          },
        ],
        response_id: "resp_quick_01",
        usage: {
          model: "claude-sonnet-4-20250514",
          prompt_tokens: 120,
          completion_tokens: 34,
          duration_ms: 890,
        },
      },
    },
    {
      id: "evt_quick_0006",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: "turn_quick_01",
      created_at: ts(25),
      data: { type: "turn_ended" },
    },
    {
      id: "evt_quick_0007",
      conversation_id: conversationId,
      session_id: "sess_quick_01",
      turn_id: null,
      created_at: ts(24),
      data: { type: "session_ended" },
    },
  ];
}

function buildRefactorEvents(conversationId: string): Event[] {
  const toolCallId = "tcall_grep_01";
  return [
    {
      id: "evt_ref_0001",
      conversation_id: conversationId,
      session_id: null,
      turn_id: null,
      created_at: ts(90),
      data: {
        type: "conversation_created",
        slug: "refactor-plan",
        name: "Refactor Plan",
      },
    },
    {
      id: "evt_ref_0002",
      conversation_id: conversationId,
      session_id: "sess_ref_01",
      turn_id: "turn_ref_01",
      created_at: ts(89),
      data: { type: "session_started" },
    },
    {
      id: "evt_ref_0003",
      conversation_id: conversationId,
      session_id: "sess_ref_01",
      turn_id: "turn_ref_01",
      created_at: ts(88),
      data: {
        type: "messages",
        messages: [
          {
            role: "user",
            content:
              "Find every callsite of `getEventsPage` in the web package.",
          },
        ],
        response_id: null,
        usage: null,
      },
    },
    {
      id: "evt_ref_0004",
      conversation_id: conversationId,
      session_id: "sess_ref_01",
      turn_id: "turn_ref_01",
      created_at: ts(87),
      data: {
        type: "tool_requested",
        tool_call_id: toolCallId,
        response_id: "resp_ref_01",
        request: {
          function_name: "grep",
          arguments: { pattern: "getEventsPage", path: "web/src" },
        },
      },
    },
    {
      id: "evt_ref_0005",
      conversation_id: conversationId,
      session_id: "sess_ref_01",
      turn_id: "turn_ref_01",
      created_at: ts(86),
      data: {
        type: "tool_result",
        tool_call_id: toolCallId,
        result: {
          matches: [
            "web/src/api/exoClient.ts",
            "web/src/api/mockClient.ts",
            "web/src/lib/useConversationEvents.ts",
          ],
          duration_ms: 64,
        },
      },
    },
    {
      id: "evt_ref_0006",
      conversation_id: conversationId,
      session_id: "sess_ref_01",
      turn_id: "turn_ref_01",
      created_at: ts(85),
      data: {
        type: "messages",
        messages: [
          {
            role: "assistant",
            content:
              "Three callsites: the real client, the mock client, and the polling hook.",
            id: "msg_ref_reply",
          },
        ],
        response_id: "resp_ref_02",
        usage: {
          model: "claude-sonnet-4-20250514",
          prompt_tokens: 310,
          completion_tokens: 28,
          duration_ms: 1120,
        },
      },
    },
  ];
}

function buildDeployEvents(conversationId: string): Event[] {
  return [
    {
      id: "evt_dep_0001",
      conversation_id: conversationId,
      session_id: null,
      turn_id: null,
      created_at: ts(15),
      data: {
        type: "conversation_created",
        slug: "deploy-review",
        name: "Deploy Review",
      },
    },
    {
      id: "evt_dep_0002",
      conversation_id: conversationId,
      session_id: "sess_dep_01",
      turn_id: "turn_dep_01",
      created_at: ts(14),
      data: {
        type: "messages",
        messages: [
          { role: "user", content: "Summarize staging deploy status." },
          {
            role: "assistant",
            content:
              "Staging is green. Last deploy `v0.4.2` finished 12 minutes ago; no rollback flags.",
            id: "msg_dep_reply",
          },
        ],
        response_id: "resp_dep_01",
        usage: {
          model: "claude-sonnet-4-20250514",
          prompt_tokens: 96,
          completion_tokens: 41,
          duration_ms: 760,
        },
      },
    },
  ];
}

export const MOCK_EVENTS_BY_CONVERSATION: Record<string, Event[]> = {
  conv_demo_rich: buildDemoRichEvents("conv_demo_rich"),
  conv_quick_check: buildQuickCheckEvents("conv_quick_check"),
  conv_refactor: buildRefactorEvents("conv_refactor"),
  conv_deploy: buildDeployEvents("conv_deploy"),
  [LARGE_CONVERSATION_ID]: LARGE_PERF_EVENTS,
};

function latestEventId(events: Event[]): string | null {
  if (events.length === 0) {
    return null;
  }
  const sorted = [...events].sort((left, right) =>
    left.id.localeCompare(right.id),
  );
  return sorted[sorted.length - 1]!.id;
}

export const MOCK_CONVERSATIONS: ConversationHandleInfo[] = [
  {
    agent_id: "agent_01H8RESEARCH0001",
    record: {
      id: "conv_demo_rich",
      slug: "protocol-demo",
      name: "Protocol Demo",
      latest_event_id: latestEventId(
        MOCK_EVENTS_BY_CONVERSATION.conv_demo_rich,
      ),
    },
  },
  {
    agent_id: "agent_01H8RESEARCH0001",
    record: {
      id: "conv_quick_check",
      slug: "quick-check",
      name: "Quick Health Check",
      latest_event_id: latestEventId(
        MOCK_EVENTS_BY_CONVERSATION.conv_quick_check,
      ),
    },
  },
  {
    agent_id: "agent_01H8RESEARCH0001",
    record: {
      id: "conv_refactor",
      slug: "refactor-plan",
      name: "Refactor Plan",
      latest_event_id: latestEventId(MOCK_EVENTS_BY_CONVERSATION.conv_refactor),
    },
  },
  {
    agent_id: "agent_01H8OPS000000001",
    record: {
      id: "conv_deploy",
      slug: "deploy-review",
      name: "Deploy Review",
      latest_event_id: latestEventId(MOCK_EVENTS_BY_CONVERSATION.conv_deploy),
    },
  },
  {
    agent_id: "agent_01H8RESEARCH0001",
    record: {
      id: LARGE_CONVERSATION_ID,
      slug: "large-perf-fixture",
      name: "Large Perf Fixture",
      latest_event_id: latestEventId(
        MOCK_EVENTS_BY_CONVERSATION[LARGE_CONVERSATION_ID],
      ),
    },
  },
];
