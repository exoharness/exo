import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";

import { api, createAgent, waitTurn, stopSandboxes } from "./live-api.mjs";

const source = (
  await readFile(
    new URL("../../examples/managed-agents/assistant.md", import.meta.url),
    "utf8",
  )
).replace("model: gpt-5.5", "model: gpt-6.1-sol");
const { agent, thread, path } = await createAgent(
  source,
  "worker-basic",
  "Worker Basic Test",
);
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
  const events = await waitTurn(path, turn.turn.id);
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
