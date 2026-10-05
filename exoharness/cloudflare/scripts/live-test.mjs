import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { api, stopSandboxes } from "./live-api.mjs";

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
  await stopSandboxes(agent, thread);
}
