import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

const base = process.env.EXO_WORKER_URL;
assert(base, "EXO_WORKER_URL is required (the Worker origin, without /exo)");
assert(process.env.EXO_TOKEN, "EXO_TOKEN is required");
async function api(path, method = "GET", body, expected = 200) {
  const response = await fetch(`${base}/exo/${path}`, {
    method,
    headers: {
      authorization: `Bearer ${process.env.EXO_TOKEN}`,
      "content-type": "application/json",
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal: AbortSignal.timeout(180_000),
  });
  const value = await response.json();
  assert.equal(response.status, expected, JSON.stringify(value));
  return value;
}
const source = (
  await readFile(
    new URL("../../examples/managed-agents/assistant.md", import.meta.url),
    "utf8",
  )
).replace("model: gpt-5.5", "model: gpt-6.1-sol");
const agent = await api("agent", "POST", {
  slug: `worker-basic-${Date.now()}`,
  name: "Worker Basic Test",
});
await api(`agent/${agent.id}/artifact`, "POST", {
  path: "managed-agents/agent.md",
  contents: [...new TextEncoder().encode(source)],
});
const { thread } = await api(`agent/${agent.id}/thread`, "POST", {});
const path = `agent/${agent.id}/thread/${thread.id}`;
console.log(`Testing ${path}`);
try {
  const turn = await api(
    `${path}/turn`,
    "POST",
    {
      input: {
        role: "user",
        content:
          "Use shell to print the contents of /etc/os-release. Then summarize the platform in one sentence.",
      },
    },
    202,
  );
  let events;
  for (let i = 0; i < 180; i++) {
    events = (await api(`${path}/event?turn_id=${turn.turn.id}&limit=1000`))
      .events;
    if (events.some((event) => event.data.type === "turn_ended")) break;
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  assert(
    events.some((event) => event.data.type === "turn_ended"),
    "turn did not finish",
  );
  assert(
    !events.some((event) => event.data.type === "error"),
    JSON.stringify(events.filter((event) => event.data.type === "error")),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "tool_result" &&
        /Debian/.test(event.data.result.stdout ?? ""),
    ),
    "shell did not report the platform",
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "messages" && event.data.usage?.prompt_tokens > 0,
    ),
    "usage is missing",
  );
  console.log(
    "PASS basic model, native shell execution, canonical events and usage",
  );
} finally {
  const result = await api("request", "POST", {
    kind: "request",
    id: 1,
    request: {
      type: "stop_sandbox",
      scope: { type: "thread", agent_id: agent.id, thread_id: thread.id },
      sandbox_id: thread.id,
    },
  });
  assert.equal(result.ok, true, result.error);
}
