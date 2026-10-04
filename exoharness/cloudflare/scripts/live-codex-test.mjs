import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";

const base =
  process.env.EXO_WORKER_URL ??
  "https://exo-managed-agents-spike.braintrust.workers.dev";
const secrets = JSON.parse(
  await readFile(new URL("../.local/secrets.json", import.meta.url), "utf8"),
);
async function api(path, method = "GET", body, expected = 200) {
  const response = await fetch(`${base}/exo/${path}`, {
    method,
    headers: {
      authorization: `Bearer ${secrets.EXO_TOKEN}`,
      "content-type": "application/json",
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal: AbortSignal.timeout(180_000),
  });
  const value = await response.json();
  assert.equal(response.status, expected, JSON.stringify(value));
  return value;
}
async function exec(path, source) {
  const result = await api(`${path}/sandbox/exec`, "POST", {
    command: ["node", "--input-type=module", "-e", source],
  });
  assert.equal(result.exitCode, 0, result.stderr);
  return result.stdout.trim();
}
const source = `---\nname: Cloudflare Codex Test\nharness: codex\nconfig:\n  model: ${process.env.EXO_CODEX_MODEL ?? "gpt-5.4"}\n  credential: global/OPENAI_API_KEY\n---\nYou are a coding agent. Work in /workspace. Implement the requested changes and run node tests. Keep the final answer concise.`;
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
await writeFile(
  new URL("../.local/codex-target.json", import.meta.url),
  JSON.stringify(
    { base, agentId: agent.id, threadId: thread.id, path },
    null,
    2,
  ),
);
console.log(`Testing ${path}`);
const checks = [];
async function turn(content) {
  const submitted = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content } },
    202,
  );
  assert.equal(submitted.harness, "codex");
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
  await writeFile(
    new URL(`../.local/codex-${submitted.turn.id}.json`, import.meta.url),
    JSON.stringify(events, null, 2),
  );
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
  assert(events.some((event) => event.data.event_type === "codex_text_delta"));
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
  assert.equal(
    await exec(
      path,
      `const {sum} = await import("/workspace/sum.mjs"); console.log(sum([]), sum([1,2,3]), sum([-3,1,2]));`,
    ),
    "0 6 0",
  );
  checks.push(
    "real Codex app-server coding turn, file edits, native shell execution, canonical history and streamed text",
  );
  assert.equal(
    await exec(
      path,
      `console.log(process.env.OPENAI_API_KEY.startsWith("exo_egress_"));`,
    ),
    "true",
  );
  assert.equal(
    await exec(
      path,
      `const r = await fetch("https://example.com"); console.log(r.status);`,
    ),
    "403",
  );
  checks.push(
    "Codex receives only a placeholder; unauthorized HTTPS remains blocked",
  );
  await api(`${path}/sandbox/stop`, "POST", {});
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
  const tests = await api(`${path}/sandbox/exec`, "POST", {
    command: ["node", "--test", "sum.test.mjs"],
  });
  assert.equal(tests.exitCode, 0, tests.stderr);
  assert.match(tests.stdout, /tests 4/);
  checks.push(
    "automatic snapshot restores workspace and native Codex history after sandbox destruction; follow-up changes pass four tests",
  );
  await writeFile(
    new URL("../.local/codex-results.json", import.meta.url),
    JSON.stringify(
      {
        base,
        agentId: agent.id,
        threadId: thread.id,
        nativeThread: first.nativeThread,
        checks,
        passedAt: new Date().toISOString(),
      },
      null,
      2,
    ),
  );
  for (const check of checks) console.log(`PASS ${check}`);
} finally {
  await api(`${path}/sandbox/stop`, "POST", {});
}
