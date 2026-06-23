import type { Event, EventData } from "./protocol";

export const LARGE_CONVERSATION_ID = "conv_large_perf";
export const DEFAULT_LARGE_EVENT_COUNT = 3000;
export const DEFAULT_LARGE_SEED = 0x4c415247;

function createSeededRng(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function tsFromIndex(index: number): string {
  const date = new Date("2026-06-18T08:00:00.000Z");
  date.setSeconds(date.getSeconds() + index);
  return date.toISOString();
}

function eventId(index: number): string {
  return `evt_lg_${String(index).padStart(5, "0")}`;
}

function pick<T>(rng: () => number, items: readonly T[]): T {
  return items[Math.floor(rng() * items.length)]!;
}

function userPrompt(turn: number, variant: number): string {
  const prompts = [
    `Turn ${turn}: summarize module boundaries in src/lib.`,
    `Turn ${turn}: list failing tests under web/src and propose fixes.`,
    `Turn ${turn}: compare pagination strategies for event timelines.`,
    `Turn ${turn}: draft a refactor plan for mockClient event paging.`,
    `Turn ${turn}: explain how artifact_written events surface in the UI.`,
  ];
  return prompts[variant % prompts.length]!;
}

function assistantPlain(turn: number, variant: number): string {
  return `Turn ${turn} reply (variant ${variant}): reviewed the request, scanned fixtures, and prepared a concise answer without touching live secrets.`;
}

function assistantRichMarkdown(turn: number, variant: number): string {
  const rows = [
    `| check | latency | ok |`,
    `|-------|---------|-----|`,
    `| load events | ${18 + (variant % 7)}ms | yes |`,
    `| render md | ${6 + (variant % 5)}ms | yes |`,
    `| tool row | ${40 + (variant % 11)}ms | yes |`,
  ].join("\n");

  return `## Turn ${turn} analysis

Throughput estimate: $R = \\frac{${120 + variant}}{\\Delta t}$ for window $\\Delta t$ seconds.

${rows}

\`\`\`typescript
export function slicePage<T extends { id: string }>(
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

\`\`\`rust
pub fn paginate_ids(ids: &[String], cursor: Option<&str>, limit: usize) -> &[String] {
    let start = cursor
        .and_then(|c| ids.iter().position(|id| id.as_str() > c).map(|i| i + 1))
        .unwrap_or(0);
    &ids[start..start.saturating_add(limit).min(ids.len())]
}
\`\`\``;
}

type TurnPattern = "plain" | "tools" | "rich" | "artifact";

function turnPattern(rng: () => number, turnIndex: number): TurnPattern {
  const bucket = turnIndex % 10;
  if (bucket === 3 || bucket === 7) {
    return "rich";
  }
  if (bucket === 5) {
    return "artifact";
  }
  if (bucket === 1 || bucket === 8) {
    return "tools";
  }
  if (rng() < 0.35) {
    return "tools";
  }
  return "plain";
}

export interface GenerateLargeConversationOptions {
  conversationId?: string;
  eventCount?: number;
  seed?: number;
}

export function generateLargeConversationEvents(
  options: GenerateLargeConversationOptions = {},
): Event[] {
  const conversationId = options.conversationId ?? LARGE_CONVERSATION_ID;
  const targetCount = options.eventCount ?? DEFAULT_LARGE_EVENT_COUNT;
  const rng = createSeededRng(options.seed ?? DEFAULT_LARGE_SEED);

  const sessionId = "sess_large_perf_001";
  const events: Event[] = [];
  let index = 1;

  const push = (
    data: EventData,
    extra?: { session_id?: string | null; turn_id?: string | null },
  ) => {
    if (events.length >= targetCount) {
      return false;
    }
    events.push({
      id: eventId(index),
      conversation_id: conversationId,
      session_id: extra?.session_id ?? null,
      turn_id: extra?.turn_id ?? null,
      created_at: tsFromIndex(index),
      data,
    });
    index += 1;
    return true;
  };

  push({
    type: "conversation_created",
    slug: "large-perf-fixture",
    name: "Large Perf Fixture",
  });
  push({ type: "session_started" }, { session_id: sessionId });

  let turnNumber = 0;
  while (events.length < targetCount - 2) {
    turnNumber += 1;
    const turnId = `turn_large_${String(turnNumber).padStart(4, "0")}`;
    const variant = turnNumber % 5;
    const pattern = turnPattern(rng, turnNumber);
    const responseId = `resp_lg_${String(turnNumber).padStart(4, "0")}`;
    const toolCallId = `tcall_lg_${String(turnNumber).padStart(4, "0")}`;

    if (
      !push(
        { type: "turn_started" },
        { session_id: sessionId, turn_id: turnId },
      )
    ) {
      break;
    }
    if (
      !push(
        {
          type: "messages",
          messages: [
            { role: "user", content: userPrompt(turnNumber, variant) },
          ],
          response_id: null,
          usage: null,
        },
        { session_id: sessionId, turn_id: turnId },
      )
    ) {
      break;
    }

    if (pattern === "tools" || pattern === "artifact" || pattern === "rich") {
      const toolName = pick(rng, ["read_file", "grep", "list_dir"] as const);
      if (
        !push(
          {
            type: "tool_requested",
            tool_call_id: toolCallId,
            response_id: responseId,
            request: {
              function_name: toolName,
              arguments: {
                path: `src/lib/module-${turnNumber % 12}.ts`,
                pattern: `perf_${turnNumber}`,
                limit: 64 + (turnNumber % 32),
              },
            },
          },
          { session_id: sessionId, turn_id: turnId },
        )
      ) {
        break;
      }
      if (
        !push(
          {
            type: "tool_result",
            tool_call_id: toolCallId,
            result: {
              path: `src/lib/module-${turnNumber % 12}.ts`,
              matches: turnNumber % 4,
              duration_ms: 30 + (turnNumber % 90),
              excerpt: `{"turn":${turnNumber},"variant":${variant}}`,
            },
          },
          { session_id: sessionId, turn_id: turnId },
        )
      ) {
        break;
      }
    }

    const assistantContent =
      pattern === "rich" || (pattern === "artifact" && turnNumber % 2 === 0)
        ? assistantRichMarkdown(turnNumber, variant)
        : assistantPlain(turnNumber, variant);

    if (
      !push(
        {
          type: "messages",
          messages: [
            {
              role: "assistant",
              content: assistantContent,
              id: `msg_lg_${String(turnNumber).padStart(4, "0")}`,
            },
          ],
          response_id: responseId,
          usage: {
            model: "claude-sonnet-4-20250514",
            prompt_tokens: 400 + (turnNumber % 200),
            completion_tokens: 80 + (turnNumber % 120),
            duration_ms: 800 + (turnNumber % 2200),
            ttft_ms: 200 + (turnNumber % 400),
            cost_usd: 0.002 + (turnNumber % 50) / 10000,
          },
        },
        { session_id: sessionId, turn_id: turnId },
      )
    ) {
      break;
    }

    if (pattern === "artifact" && turnNumber % 3 !== 0) {
      const artifactId = `art_lg_${String(turnNumber).padStart(4, "0")}`;
      if (
        !push(
          {
            type: "artifact_written",
            artifact_id: artifactId,
            path: `reports/turn-${String(turnNumber).padStart(4, "0")}.md`,
            version: 1,
          },
          { session_id: sessionId, turn_id: turnId },
        )
      ) {
        break;
      }
    }

    if (
      !push({ type: "turn_ended" }, { session_id: sessionId, turn_id: turnId })
    ) {
      break;
    }
  }

  if (events.length < targetCount) {
    push({ type: "session_ended" }, { session_id: sessionId });
  }

  while (events.length < targetCount) {
    push(
      {
        type: "custom",
        event_type: "perf_padding",
        payload: { index: events.length + 1 },
      },
      { session_id: sessionId },
    );
  }

  return events.slice(0, targetCount);
}

export const LARGE_PERF_EVENTS: Event[] = generateLargeConversationEvents();
