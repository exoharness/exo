import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { api, stopSandboxes } from "./live-api.mjs";

const source = (
  await readFile(
    new URL("../../examples/managed-agents/coder.md", import.meta.url),
    "utf8",
  )
).replace(
  "model: gpt-6.1-sol",
  `model: ${process.env.EXO_CODEX_MODEL ?? "gpt-6.1-sol"}`,
);
const agent = await api("agent", "POST", {
  slug: `cloudflare-codex-${Date.now()}`,
  name: "Cloudflare Codex Test",
});
await api(`agent/${agent.id}/artifact`, "POST", {
  path: "managed-agents/agent.md",
  contents: [...new TextEncoder().encode(source)],
});
const { thread } = await api(`agent/${agent.id}/thread`, "POST", {});
const path = `agent/${agent.id}/thread/${thread.id}`;
console.log(`Testing ${path}`);
const checks = [];
async function turn(content) {
  const submitted = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content } },
    202,
  );
  assert.equal(submitted.harness, "codex-harness");
  let events;
  for (let i = 0; i < 600; i++) {
    events = (
      await api(`${path}/event?turn_id=${submitted.turn.id}&limit=1000`)
    ).events;
    if (events.some((event) => event.data.type === "turn_ended")) break;
    if (i % 20 === 0)
      console.log(`Waiting for Codex (${i}s, ${events.length} events)`);
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  assert(
    events.some((event) => event.data.type === "turn_ended"),
    "Codex turn did not finish",
  );
  assert(
    !events.some((event) => event.data.type === "error"),
    JSON.stringify(events.filter((event) => event.data.type === "error")),
  );
  const started = events.find(
    (event) => event.data.event_type === "codex_turn_started",
  );
  assert(started, "native Codex turn was not started");
  assert(
    events.some((event) => event.data.event_type === "codex_turn_completed"),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "tool_result" && event.data.result.exit_code === 0,
    ),
    "Codex did not run a successful shell command",
  );
  const usage = events
    .filter(
      (event) =>
        event.turn_id === submitted.turn.id && event.data.type === "messages",
    )
    .map((event) => event.data.usage)
    .filter(Boolean);
  assert.equal(usage.length, 1, "expected one canonical usage event per turn");
  assert(usage[0].prompt_tokens > 0, "Codex input token counts are missing");
  assert(
    usage[0].completion_tokens > 0,
    "Codex output token counts are missing",
  );
  assert(Number.isFinite(usage[0].cost_usd), "Codex cost is missing");
  const markers = new Map(
    events.map((event) => [event.data.event_type ?? event.data.type, event]),
  );
  console.log(
    JSON.stringify({
      turn: submitted.turn.id,
      duration_ms:
        Date.parse(markers.get("turn_ended").created_at) -
        Date.parse(markers.get("turn_started").created_at),
      warm_app_server_reused: started.data.payload.warm_app_server_reused,
      warm_thread_reused: started.data.payload.warm_thread_reused,
      usage: usage[0],
    }),
  );
  return {
    events,
    nativeThread: started.data.payload.codex_thread_id,
    hydrated: started.data.payload.hydrated_from,
  };
}
try {
  const tag = `phase-${crypto.randomUUID()}`;
  const first = await turn(
    `Create sum.mjs exporting sum(values), with sum([]) equal to 0. Create sum.test.mjs using node:test and strict assert with three cases (empty, positive, mixed negative). Run node --test sum.test.mjs and fix any failures. Remember this phase tag in our conversation: ${tag}. Do not write the tag into a file.`,
  );
  const warm = await turn(
    "Run node --test sum.test.mjs again. Include the phase tag from our conversation in your final answer. Do not use any network tools.",
  );
  assert.equal(warm.nativeThread, first.nativeThread);
  const warmStart = warm.events.find(
    (event) => event.data.event_type === "codex_turn_started",
  );
  assert.equal(warmStart.data.payload.warm_app_server_reused, true);
  assert.equal(warmStart.data.payload.warm_thread_reused, true);
  checks.push(
    "consecutive turns reuse the live Codex process and native thread",
  );
  await stopSandboxes(agent, thread, true);
  const second = await turn(
    "Add a fourth test for decimal inputs to the existing sum.test.mjs. Run the tests. In your final answer, include the phase tag I gave you in our earlier message. Do not use any network tools.",
  );
  assert.equal(
    second.nativeThread,
    first.nativeThread,
    "Codex resumed a different native thread",
  );
  assert.equal(second.hydrated, "warm_codex_thread");
  assert(
    second.events.some(
      (event) =>
        event.data.type === "messages" &&
        JSON.stringify(event.data.messages).includes(tag),
    ),
    "Codex lost conversation history across sandbox destruction",
  );
  assert(
    second.events.some(
      (event) =>
        event.data.type === "tool_result" &&
        /tests 4/.test(event.data.result.output ?? ""),
    ),
    "four tests did not pass",
  );
  checks.push(
    "explicit snapshot restores workspace and native Codex history after sandbox destruction; resumed usage is reported and follow-up changes pass four tests",
  );
  for (const check of checks) console.log(`PASS ${check}`);
} finally {
  await stopSandboxes(agent, thread);
}
