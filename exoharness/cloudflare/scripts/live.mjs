import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { runContracts } from "./contract-test.mjs";

const mode = process.argv[2];
assert(
  ["basic", "codex", "contracts"].includes(mode),
  "usage: node scripts/live.mjs <basic|codex|contracts>",
);

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
async function rpc(request) {
  const result = await api("request", "POST", {
    kind: "request",
    id: 1,
    request,
  });
  assert.equal(result.ok, true, result.error);
  return result.response;
}
async function createAgent(source, slug, name, model) {
  const agent = await api("agent", "POST", {
    slug: `${slug}-${Date.now()}`,
    name,
  });
  await api(`agent/${agent.id}/artifact`, "POST", {
    path: "managed-agents/agent.md",
    contents: [...new TextEncoder().encode(source)],
  });
  const { thread } = await api(`agent/${agent.id}/thread`, "POST", { model });
  const path = `agent/${agent.id}/thread/${thread.id}`;
  console.log(`Testing ${path}`);
  return { agent, thread, path };
}
async function waitTurn(path, turnId, attempts = 180) {
  for (let i = 0; i < attempts; i++) {
    const { events } = await api(`${path}/event?turn_id=${turnId}&limit=1000`);
    if (events.some((event) => event.data.type === "turn_ended")) {
      assert(
        !events.some((event) => event.data.type === "error"),
        JSON.stringify(events.filter((event) => event.data.type === "error")),
      );
      return events;
    }
    if (i % 20 === 0)
      console.log(`Waiting for turn (${i}s, ${events.length} events)`);
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  throw new Error("turn did not finish");
}
async function stopSandboxes(agent, thread, restore = false) {
  const scope = { type: "thread", agent_id: agent.id, thread_id: thread.id };
  const result = await rpc({ type: "list_sandboxes", scope });
  for (const sandbox of result.sandboxes) {
    if (!sandbox.running) continue;
    const snapshot = restore
      ? await rpc({
          type: "snapshot_sandbox",
          scope: { type: "resource", scope },
          sandbox_id: sandbox.id,
        })
      : null;
    await rpc({ type: "stop_sandbox", scope, sandbox_id: sandbox.id });
    if (snapshot)
      await rpc({
        type: "start_sandbox",
        scope: { type: "resource", scope },
        request: {
          id: sandbox.id,
          snapshot_id: snapshot.snapshot_id,
          idle_seconds: null,
          provider: null,
        },
      });
  }
}

async function basic() {
  const source = await readFile(
    new URL("../../examples/managed-agents/assistant.md", import.meta.url),
    "utf8",
  );
  const { agent, thread, path } = await createAgent(
    source,
    "worker-basic",
    "Worker Basic Test",
    "gpt-6.1-sol",
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
    console.log("Basic turn passed.");
  } finally {
    await stopSandboxes(agent, thread);
  }
}
async function codex() {
  const source = await readFile(
    new URL("../../examples/managed-agents/coder.md", import.meta.url),
    "utf8",
  );
  const { agent, thread, path } = await createAgent(
    source,
    "cloudflare-codex",
    "Cloudflare Codex Test",
    process.env.EXO_CODEX_MODEL ?? "gpt-6.1-sol",
  );
  async function turn(content) {
    const submitted = await api(
      `${path}/turn`,
      "POST",
      { input: { role: "user", content } },
      202,
    );
    assert.equal(submitted.harness, "codex");
    const events = await waitTurn(path, submitted.turn.id, 600);
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
          event.data.type === "tool_result" &&
          event.data.result.exit_code === 0,
      ),
      "Codex did not run a successful shell command",
    );
    const usage = events.find((event) => event.data.usage)?.data.usage;
    assert(usage?.prompt_tokens > 0, "Codex input token counts are missing");
    assert(
      usage.completion_tokens > 0,
      "Codex output token counts are missing",
    );
    assert(Number.isFinite(usage.cost_usd), "Codex cost is missing");
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
    console.log("Codex reuse and snapshot restore passed.");
  } finally {
    await stopSandboxes(agent, thread);
  }
}

if (mode === "contracts")
  console.log(await runContracts(`${base}/exo`, process.env.EXO_TOKEN));
else if (mode === "basic") await basic();
else await codex();
