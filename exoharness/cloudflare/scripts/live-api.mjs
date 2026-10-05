import assert from "node:assert/strict";

export const base = process.env.EXO_WORKER_URL;
assert(base, "EXO_WORKER_URL is required (the Worker origin, without /exo)");
assert(process.env.EXO_TOKEN, "EXO_TOKEN is required");
export async function api(path, method = "GET", body, expected = 200) {
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
export async function rpc(request) {
  const result = await api("request", "POST", {
    kind: "request",
    id: 1,
    request,
  });
  assert.equal(result.ok, true, result.error);
  return result.response;
}
export async function stopSandboxes(agent, thread, restore = false) {
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
