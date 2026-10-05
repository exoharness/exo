import assert from "node:assert/strict";
import { readFile, readdir, mkdtemp, rm } from "node:fs/promises";
import { resolve } from "node:path";
import { after, before, test } from "node:test";
import { Miniflare } from "miniflare";
import { stripTypeScriptTypes } from "node:module";
import { harnessContracts } from "../../typescript/harness/contracts.ts";
import { fakeCodex } from "./fixtures/fake-codex.mjs";

const token = "test-operator-token";
let mf;
let persistence;
const key = "synthetic-model-key";
let modelRequests = 0;

const fakeSandbox = `import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";
${fakeCodex}
export class FakeSandbox extends DurableObject {
  async exec(identity, request) {
    const count = (await this.ctx.storage.get("count") ?? 0) + 1;
    await this.ctx.storage.put("count", count);
    return {stdout: "Linux test\\n", stderr: "", exitCode: 0};
  }
  async count() { return await this.ctx.storage.get("count") ?? 0; }
  async startProcess() { return new FakeCodexProcess(this.ctx.storage); }
  async runProcess(identity, request, ready) { const process = await this.startProcess(); await ready(new RpcStub(process)); await process.wait(); }
  async prepareCodex() {}
  async snapshot() { return {id:"fixture-snapshot", size:1}; }
  async lastMethod() { return await this.ctx.storage.get("last-method"); }
  async stop() {}
}
export default { fetch() { return new Response("fixture"); } };`;

async function options({ accessAud, access, staticToken = token } = {}) {
  const modules = {
    "index.js": {
      type: "esm",
      contents: `import { ExoProvider as Provider } from "./implementation.js";
import { harnessContracts, seedHarnessCheckpoint, verifyHarnessCheckpoint } from "./contracts.js";
export class ExoProvider extends Provider {
  async recoverForTest() { await this.alarm(); }
  fixture() {
    return {
      harness: this.harness,
      beginTurn: async (agent, conversation, input, sessionId) => {
        const turn = await this.harness.beginTurn(agent.record.id, conversation.record.id, input.length ? [{type:"messages", messages:input}] : [], sessionId);
        return this.harness.forTurn(agent.record.id, conversation.record.id, turn.record);
      },
      finishTurn: turn => turn.finish(),
    };
  }
  async runContract(name) {
    try { await harnessContracts[name](this.fixture()); return JSON.stringify({ok:true}); }
    catch(error) { return JSON.stringify({ok:false, error:error.message, stack:error.stack}); }
  }
  async seedCheckpoint() { return JSON.stringify(await seedHarnessCheckpoint(this.fixture())); }
  async verifyCheckpoint(saved) {
    try { await verifyHarnessCheckpoint(this.fixture(), JSON.parse(saved)); return JSON.stringify({ok:true}); }
    catch(error) { return JSON.stringify({ok:false, error:error.message, stack:error.stack}); }
  }
}
export {default, ExoSandbox, ExoEgress} from "./implementation.js";`,
    },
    "contracts.js": {
      type: "esm",
      contents: stripTypeScriptTypes(
        await readFile(
          new URL("../../typescript/harness/contracts.ts", import.meta.url),
          "utf8",
        ),
      ),
    },
    "implementation.js": {
      type: "esm",
      contents: await readFile(
        new URL("../dist/index.js", import.meta.url),
        "utf8",
      ),
    },
  };
  for (const file of await readdir(new URL("../dist/", import.meta.url))) {
    if (file.endsWith("-version"))
      modules[file] = {
        type: "text",
        contents: await readFile(
          new URL(`../dist/${file}`, import.meta.url),
          "utf8",
        ),
      };
    if (file.endsWith(".wasm"))
      modules[file] = {
        type: "wasm",
        contents: new Uint8Array(
          await readFile(new URL(`../dist/${file}`, import.meta.url)),
        ),
      };
  }
  return {
    resourcePersistencePath: persistence,
    unsafeInspectDurableObjects: true,
    telemetry: { enabled: false },
    workers: [
      {
        config: {
          name: "exo",
          compatibilityDate: "2026-10-04",
          compatibilityFlags: ["nodejs_compat"],
          manifest: { mainModule: "index.js", modules },
          env: {
            PROVIDERS: {
              type: "durable-object",
              worker: "exo",
              exportName: "ExoProvider",
            },
            SANDBOXES: {
              type: "durable-object",
              worker: "sandbox",
              exportName: "FakeSandbox",
            },
            ARTIFACTS: { type: "r2", name: "artifacts" },
            ACCOUNT_ID: { type: "json", value: "test-account" },
            ...(staticToken === null
              ? {}
              : { EXO_TOKEN: { type: "json", value: staticToken } }),
            ...(accessAud === undefined
              ? {}
              : { ACCESS_AUD: { type: "json", value: accessAud } }),
            VAULT_KEY: { type: "json", value: "ab".repeat(32) },
          },
          exports: {
            ExoProvider: { type: "durable-object", storage: "sqlite" },
          },
        },
        dev: {
          access,
          outboundService: {
            type: "fetcher",
            handler: async (request) => {
              if (new URL(request.url).hostname !== "model.example")
                return Response.json(
                  {
                    header: request.headers.get("authorization"),
                    redirect: request.redirect,
                  },
                  {
                    status: 302,
                    headers: { location: "https://other.example/" },
                  },
                );
              assert.equal(
                request.headers.get("authorization"),
                `Bearer ${key}`,
              );
              modelRequests += 1;
              const body = await request.json();
              const lastUser = body.input.findLastIndex(
                (item) => item.role === "user",
              );
              const hasResult = body.input
                .slice(lastUser + 1)
                .some((item) => item.type === "function_call_output");
              return Response.json({
                id: "resp_test",
                object: "response",
                status: "completed",
                model: "test-model",
                output: hasResult
                  ? [
                      {
                        type: "message",
                        id: "msg_test",
                        role: "assistant",
                        status: "completed",
                        content: [
                          {
                            type: "output_text",
                            text: "Done.",
                            annotations: [],
                          },
                        ],
                      },
                    ]
                  : [
                      {
                        type: "function_call",
                        id: "fc_test",
                        status: "completed",
                        call_id: "call_test",
                        name: "shell",
                        arguments: JSON.stringify({ command: "uname -s" }),
                      },
                    ],
                usage: {
                  input_tokens: 10,
                  output_tokens: 5,
                  total_tokens: 15,
                  input_tokens_details: { cached_tokens: 0 },
                  output_tokens_details: { reasoning_tokens: 0 },
                },
              });
            },
          },
        },
      },
      {
        config: {
          name: "sandbox",
          compatibilityDate: "2026-10-04",
          manifest: {
            mainModule: "index.js",
            modules: { "index.js": { type: "esm", contents: fakeSandbox } },
          },
          exports: {
            FakeSandbox: { type: "durable-object", storage: "sqlite" },
          },
        },
      },
    ],
  };
}

async function api(path, method = "GET", body, expected = 200) {
  const response = await mf.dispatchFetch(`https://exo.test/exo/${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json();
  assert.equal(response.status, expected, JSON.stringify(value));
  return value;
}

const policy = (origin) => ({
  networking: {
    type: "destinations",
    allowed_destinations: [{ type: "origin", origin }],
  },
  injection_location: { header: true },
});
const definition = (ask) =>
  `---\nname: Test Agent\nharness: basic\nconfig:\n  model: test-model\n  base_url: https://model.example/v1\n  credential: OPENAI_API_KEY\npermission_policy: {type: ${ask ? "always_ask" : "always_allow"}}\n---\nUse shell to print Linux, then finish.`;
async function create(ask = false, harness = "basic") {
  const agent = await api("agent", "POST", {
    slug: `agent-${crypto.randomUUID()}`,
    name: "Test Agent",
  });
  await api(`agent/${agent.id}/artifact`, "POST", {
    path: "managed-agents/agent.md",
    contents: [
      ...new TextEncoder().encode(
        definition(ask).replace("harness: basic", `harness: ${harness}`),
      ),
    ],
  });
  const { thread } = await api(`agent/${agent.id}/thread`, "POST", {});
  return { agent, thread, path: `agent/${agent.id}/thread/${thread.id}` };
}
async function waitEvents(path, predicate) {
  let last;
  for (let i = 0; i < 150; i++) {
    const { events } = await api(`${path}/event?limit=1000`);
    last = events;
    if (predicate(events)) return events;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail(`timed out waiting for events: ${JSON.stringify(last)}`);
}

before(async () => {
  persistence = await mkdtemp(resolve(".local/test-"));
  mf = new Miniflare(await options());
  await mf.ready;
  const vault = await api("vault", "POST", { name: "global" });
  await api(`vault/${vault.id}/secret`, "POST", {
    name: "OPENAI_API_KEY",
    secret: { type: "key", value: key },
    policy: policy("https://model.example"),
  });
});
after(async () => {
  await mf?.dispose();
  if (persistence) await rm(persistence, { recursive: true, force: true });
});

test("bearer authentication protects every Exo route", async () => {
  assert.equal(
    (await mf.dispatchFetch("https://exo.test/exo/identity")).status,
    401,
  );
  for (const authorization of [
    "Bearer wrong-token",
    "Basic test-operator-token",
    "Bearer ",
  ])
    assert.equal(
      (
        await mf.dispatchFetch("https://exo.test/exo/identity", {
          headers: { authorization },
        })
      ).status,
      401,
    );
  assert.deepEqual(await api("identity"), { account_id: "test-account" });
});

test("Access requires the configured platform audience and ignores identity headers", async () => {
  for (const access of [undefined, { aud: "other-app" }]) {
    const worker = new Miniflare(
      await options({ accessAud: "exo-app", access }),
    );
    try {
      for (const path of ["identity", "agent", "request"]) {
        const response = await worker.dispatchFetch(
          `https://exo.test/exo/${path}`,
          {
            headers: {
              authorization: `Bearer ${token}`,
              "Cf-Access-Jwt-Assertion": "forged-jwt",
              "Cf-Access-Authenticated-User-Email": "admin@example.com",
            },
          },
        );
        assert.equal(response.status, 403);
      }
      // The static token is configured and valid, but cannot bypass Access
      // through a request forwarded directly to the Durable Object.
      const { PROVIDERS } = await worker.getBindings("exo");
      const direct = await PROVIDERS.getByName("test-account").fetch(
        new Request("https://exo.test/exo/identity", {
          headers: { authorization: `Bearer ${token}` },
        }),
      );
      assert.equal(direct.status, 401);
    } finally {
      await worker.dispose();
    }
  }
});

test("missing authentication configuration fails closed", async () => {
  const worker = new Miniflare(await options({ staticToken: null }));
  try {
    assert.equal(
      (await worker.dispatchFetch("https://exo.test/exo/identity")).status,
      401,
    );
  } finally {
    await worker.dispose();
  }
});

test("Access forwards HTTP bodies and SSE over the trusted Durable Object binding", async () => {
  const worker = new Miniflare(
    await options({
      accessAud: "exo-app",
      access: { aud: "exo-app", identity: { email: "operator@example.com" } },
      staticToken: null,
    }),
  );
  try {
    const identity = await worker.dispatchFetch(
      "https://exo.test/exo/identity",
    );
    assert.equal(identity.status, 200);
    assert.deepEqual(await identity.json(), { account_id: "test-account" });
    const created = await worker.dispatchFetch("https://exo.test/exo/agent", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ slug: "access-agent", name: "Access agent" }),
    });
    assert.equal(created.status, 200);
    const agent = await created.json();
    const listed = await worker.dispatchFetch("https://exo.test/exo/agent");
    assert.equal(listed.status, 200);
    assert.equal((await listed.json()).agents[0].id, agent.id);
    const artifact = await worker.dispatchFetch(
      `https://exo.test/exo/agent/${agent.id}/artifact`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          path: "managed-agents/agent.md",
          contents: [...new TextEncoder().encode(definition(false))],
        }),
      },
    );
    assert.equal(artifact.status, 200);
    const opened = await worker.dispatchFetch(
      `https://exo.test/exo/agent/${agent.id}/thread`,
      { method: "POST", body: "{}" },
    );
    assert.equal(opened.status, 200);
    const { thread } = await opened.json();
    const stream = await worker.dispatchFetch(
      `https://exo.test/exo/agent/${agent.id}/thread/${thread.id}/event/watch`,
    );
    assert.equal(stream.status, 200);
    assert.equal(stream.headers.get("content-type"), "text/event-stream");
    const reader = stream.body.getReader();
    try {
      const { value, done } = await reader.read();
      assert.equal(done, false);
      assert.match(new TextDecoder().decode(value), /event: exo_event/);
    } finally {
      await reader.cancel();
    }
  } finally {
    await worker.dispose();
  }
});

test("managed basic turn calls the model, executes one tool and persists canonical events", async () => {
  const { thread, path } = await create();
  const submitted = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "Print Linux." } },
    202,
  );
  const events = await waitEvents(path, (events) =>
    events.some((event) => event.data.type === "turn_ended"),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "tool_result" &&
        event.data.result.stdout === "Linux test\n",
    ),
    JSON.stringify(events),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "messages" &&
        event.data.messages.some((message) =>
          message.content.some?.((part) => part.text === "Done."),
        ),
    ),
  );
  assert.deepEqual(await api(`${path}/turn/${submitted.turn.id}`), {
    active: false,
  });
  const { SANDBOXES } = await mf.getBindings("exo");
  assert.equal(await SANDBOXES.getByName(thread.id).count(), 1);
  assert.equal(modelRequests, 2);
  for (let i = 1; i < events.length; i++)
    assert(events[i - 1].id < events[i].id);
});

test("approval pauses durably, rejects the wrong session and executes only after approval", async () => {
  const { thread, path } = await create(true);
  const submitted = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "Print Linux." } },
    202,
  );
  const events = await waitEvents(path, (events) =>
    events.some(
      (event) => event.data.event_type === "agent_runtime.approval_requested",
    ),
  );
  const approval = events.find(
    (event) => event.data.event_type === "agent_runtime.approval_requested",
  ).data.payload;
  const { SANDBOXES } = await mf.getBindings("exo");
  assert.equal(await SANDBOXES.getByName(thread.id).count(), 0);
  await api(
    `${path}/turn/${submitted.turn.id}/approval-response`,
    "POST",
    { session_id: "wrong", approval_id: approval.approval_id, approved: true },
    400,
  );
  await api(`${path}/turn/${submitted.turn.id}/approval-response`, "POST", {
    session_id: submitted.turn.session_id,
    approval_id: approval.approval_id,
    approved: true,
  });
  await waitEvents(path, (events) =>
    events.some((event) => event.data.type === "turn_ended"),
  );
  assert.equal(await SANDBOXES.getByName(thread.id).count(), 1);
});

test("artifact versions, event cursors and encrypted vaults survive a runtime restart", async () => {
  const { agent, path } = await create();
  const first = await api(`${path}/artifact`, "POST", {
    path: "result.bin",
    contents: [0, 255, 10],
  });
  const second = await api(`${path}/artifact`, "POST", {
    path: "result.bin",
    contents: [1, 2],
  });
  assert.equal(first.artifact_id, second.artifact_id);
  assert.equal(second.version, 2);
  const eventsBefore = (await api(`${path}/event`)).events;
  await mf.dispose();
  mf = new Miniflare(await options());
  await mf.ready;
  assert.deepEqual(
    (
      await api(
        `${path}/artifact/read?artifact_id=${first.artifact_id}&version=1`,
      )
    ).contents,
    [0, 255, 10],
  );
  assert.deepEqual(
    (await api(`${path}/event?after=${eventsBefore[0].id}`)).events,
    eventsBefore.slice(1),
  );
  assert.equal((await api(`agent/${agent.id}`)).id, agent.id);
  const vault = (await api("vault"))[0];
  const secrets = await api(`vault/${vault.id}/secret`);
  assert.equal(secrets[0].name, "OPENAI_API_KEY");
  assert(!JSON.stringify(secrets).includes(key));
  await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "Print Linux." } },
    202,
  );
  await waitEvents(path, (events) =>
    events.some((event) => event.data.type === "turn_ended"),
  );
});

test("proxy replaces only scoped placeholders, checks destination and returns redirects without following", async () => {
  const { agent, thread, path } = await create();
  const vault = (await api("vault"))[0];
  await api(`vault/${vault.id}/secret`, "POST", {
    name: "PROBE",
    secret: { type: "key", value: "synthetic-probe-key" },
    policy: policy("https://echo.example"),
  });
  await api(`${path}/sandbox/policy`, "PUT", {
    origins: [
      "https://echo.example",
      "http://echo.example",
      "https://other.example",
    ],
    credentials: [{ environment_variable: "PROBE_KEY", credential: "PROBE" }],
  });
  const configured = await api(`${path}/sandbox/policy`);
  const placeholder = configured.credentials[0].placeholder;
  const { PROVIDERS } = await mf.getBindings("exo");
  const provider = PROVIDERS.getByName("test-account");
  const identity = { agentId: agent.id, threadId: thread.id };
  const send = (url, value, name = "authorization") =>
    provider.proxy(identity, new Request(url, { headers: { [name]: value } }));
  const response = await send(
    "https://echo.example/check",
    `Bearer ${placeholder}`,
  );
  assert.equal(response.status, 302);
  assert.equal((await response.json()).header, "Bearer synthetic-probe-key");
  assert.equal(
    (
      await (
        await send(
          "https://echo.example/check",
          `Basic ${btoa(`user:${placeholder}`)}`,
        )
      ).json()
    ).header,
    `Basic ${btoa("user:synthetic-probe-key")}`,
  );
  await assert.rejects(
    send("https://other.example/check", `Bearer ${placeholder}`),
  );
  await assert.rejects(send("https://denied.example/check", "none"));
  await assert.rejects(
    send("http://echo.example/check", `Bearer ${placeholder}`),
  );
  await assert.rejects(
    send("https://echo.example/check", `Bearer ${PLACEHOLDER_PREFIX}unknown`),
  );
  await assert.rejects(send("https://echo.example/check", placeholder, "host"));
  const other = await create();
  await assert.rejects(
    provider.proxy(
      { agentId: other.agent.id, threadId: other.thread.id },
      new Request("https://echo.example/check", {
        headers: { authorization: `Bearer ${placeholder}` },
      }),
    ),
  );
  const id = (await api(`vault/${vault.id}/secret`)).find(
    (secret) => secret.name === "PROBE",
  ).id;
  await api(`vault/${vault.id}/secret/${id}`, "DELETE");
  await assert.rejects(
    send("https://echo.example/check", `Bearer ${placeholder}`),
  );
});

const PLACEHOLDER_PREFIX = "exo_egress_";
test("unsupported agent features and invalid origins fail explicitly", async () => {
  const { agent, path } = await create();
  await api(
    `${path}/sandbox/policy`,
    "PUT",
    { origins: ["http://127.0.0.1"], credentials: [] },
    400,
  );
  await api(
    `${path}/sandbox/policy`,
    "PUT",
    { origins: ["https://echo.example/path"], credentials: [] },
    400,
  );
  await api(
    `agent/${agent.id}/artifact`,
    "POST",
    {
      path: "managed-agents/agent.md",
      contents: [
        ...new TextEncoder().encode(
          definition(false).replace("harness: basic", "harness: claude"),
        ),
      ],
    },
    400,
  );
});

test("pending approvals survive eviction; an ambiguous tool execution is ended without replay", async () => {
  const { thread, path } = await create(true);
  const submitted = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "Print Linux." } },
    202,
  );
  const events = await waitEvents(path, (events) =>
    events.some(
      (event) => event.data.event_type === "agent_runtime.approval_requested",
    ),
  );
  const approval = events.find(
    (event) => event.data.event_type === "agent_runtime.approval_requested",
  ).data.payload;
  await mf.dispose();
  mf = new Miniflare(await options());
  await mf.ready;
  assert.deepEqual(await api(`${path}/turn/${submitted.turn.id}`), {
    active: true,
  });
  assert(
    (await api(`${path}/event`)).events.some(
      (event) => event.data.payload?.approval_id === approval.approval_id,
    ),
  );
  const storage = await mf.unsafeGetDurableObjectStorage("exo", "ExoProvider", {
    name: "test-account",
  });
  const requested = events.find(
    (event) => event.data.event_type === "agent_runtime.approval_requested",
  );
  // An unresolved tool call without a pending approval has an unknown outcome.
  // Remove only the pending-approval marker from the persisted canonical log.
  requested.data.event_type = "fixture.removed_approval";
  await storage.exec(
    "UPDATE state SET json = ? WHERE key = ?",
    JSON.stringify({
      id: requested.id,
      conversationId: thread.id,
      sessionId: requested.session_id,
      turnId: requested.turn_id,
      createdAt: requested.created_at,
      data: requested.data,
    }),
    `event/${thread.id}/${requested.id}`,
  );
  await mf.dispose();
  mf = new Miniflare(await options());
  await mf.ready;
  const { PROVIDERS, SANDBOXES } = await mf.getBindings("exo");
  await PROVIDERS.getByName("test-account").recoverForTest();
  const recovered = (await api(`${path}/event`)).events;
  assert(
    recovered.some(
      (event) =>
        event.data.type === "error" &&
        event.data.message.includes("unresolved tool call"),
    ),
  );
  assert(recovered.some((event) => event.data.type === "turn_ended"));
  assert.equal(await SANDBOXES.getByName(thread.id).count(), 0);
});

test("shared runtime serializes concurrent submissions and cancellation prevents pending tools", async () => {
  const { thread, path } = await create(true);
  const first = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "Print Linux." } },
    202,
  );
  const second = api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "Print Linux again." } },
    202,
  );
  await waitEvents(path, (events) =>
    events.some(
      (event) => event.data.event_type === "agent_runtime.approval_requested",
    ),
  );
  assert.equal(
    (await api(`${path}/turn/${first.turn.id}/cancel`, "POST", {}))
      .canceled_active_turn,
    true,
  );
  const submitted = await second;
  await waitEvents(path, (events) =>
    events.some(
      (event) =>
        event.turn_id === submitted.turn.id &&
        event.data.event_type === "agent_runtime.approval_requested",
    ),
  );
  assert.equal(
    (await api(`${path}/turn/${submitted.turn.id}/cancel`, "POST", {}))
      .canceled_active_turn,
    true,
  );
  await waitEvents(path, (events) =>
    events.some(
      (event) =>
        event.turn_id === submitted.turn.id && event.data.type === "turn_ended",
    ),
  );
  assert.deepEqual(await api(`${path}/turn/${submitted.turn.id}`), {
    active: false,
  });
  const { SANDBOXES } = await mf.getBindings("exo");
  assert.equal(await SANDBOXES.getByName(thread.id).count(), 0);
  const events = (await api(`${path}/event`)).events;
  assert.equal(
    events.filter((event) => event.data.type === "turn_started").length,
    2,
  );
});

test("SSE replays a cursor and streams new events without duplicates", async () => {
  const { path } = await create();
  const first = (await api(`${path}/event`)).events[0];
  const abort = new AbortController();
  const response = await mf.dispatchFetch(
    `https://exo.test/exo/${path}/event/watch?after=${first.id}`,
    { headers: { authorization: `Bearer ${token}` }, signal: abort.signal },
  );
  assert.equal(response.headers.get("content-type"), "text/event-stream");
  const reader = response.body.getReader();
  const received = [];
  const collect = (async () => {
    let pending = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      pending += new TextDecoder().decode(value);
      let end;
      while ((end = pending.indexOf("\n\n")) >= 0) {
        const frame = pending.slice(0, end);
        pending = pending.slice(end + 2);
        const data = frame
          .split("\n")
          .find((line) => line.startsWith("data: "));
        if (data) {
          const event = JSON.parse(data.slice(6));
          received.push(event);
          if (event.data.type === "turn_ended") {
            await reader.cancel();
            return;
          }
        }
      }
    }
  })();
  try {
    await api(
      `${path}/turn`,
      "POST",
      { input: { role: "user", content: "Print Linux." } },
      202,
    );
    await collect;
    assert(received.some((event) => event.data.type === "tool_result"));
    assert(!received.some((event) => event.id === first.id));
    assert.equal(
      new Set(received.map((event) => event.id)).size,
      received.length,
    );
  } finally {
    abort.abort();
  }
});

test("vault contents are encrypted at rest and origin rules cannot silently broaden a path", async () => {
  const storage = await mf.unsafeGetDurableObjectStorage("exo", "ExoProvider", {
    name: "test-account",
  });
  const rows = await storage.exec(
    "SELECT json FROM state WHERE substr(key, 1, 7) = ?",
    "secret/",
  );
  assert(rows.length);
  assert(!JSON.stringify(rows).includes(key));
  assert(rows.every((row) => Array.isArray(JSON.parse(row.json).ciphertext)));
  const vault = (await api("vault"))[0];
  await api(
    `vault/${vault.id}/secret`,
    "POST",
    {
      name: "INVALID",
      secret: { type: "key", value: "synthetic" },
      policy: policy("https://model.example/private"),
    },
    400,
  );
});

test("Codex uses JSONL over RPC streams, persists tools and resumes the native thread", async () => {
  const { thread, path } = await create(false, "codex");
  const first = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "run tests" } },
    202,
  );
  assert.equal(first.harness, "codex");
  const events = await waitEvents(path, (events) =>
    events.some((event) => event.data.type === "turn_ended"),
  );
  assert(
    !events.some((event) => event.data.type === "error"),
    JSON.stringify(events),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "tool_result" && event.data.result.exit_code === 0,
    ),
  );
  assert(events.some((event) => event.data.event_type === "codex_text_delta"));
  const sandbox = await mf.getDurableObjectNamespace("SANDBOXES", "exo");
  assert.equal(await sandbox.getByName(thread.id).lastMethod(), "thread/start");
  const policy = await api(`${path}/sandbox/policy`);
  assert.deepEqual(policy.origins, ["https://model.example"]);
  assert(policy.credentials[0].placeholder.startsWith("exo_egress_"));
  const second = await api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content: "continue" } },
    202,
  );
  const finished = await waitEvents(path, (events) =>
    events.some(
      (event) =>
        event.turn_id === second.turn.id && event.data.type === "turn_ended",
    ),
  );
  assert(
    !finished.some((event) => event.data.type === "error"),
    JSON.stringify(finished),
  );
  assert.equal(
    await sandbox.getByName(thread.id).lastMethod(),
    "thread/resume",
  );
  const started = finished.find(
    (event) =>
      event.turn_id === second.turn.id &&
      event.data.event_type === "codex_turn_started",
  );
  assert.equal(started.data.payload.hydrated_from, "warm_codex_thread");
  for (const id of [first.turn.id, second.turn.id]) {
    const usage = finished.filter(
      (event) =>
        event.turn_id === id &&
        event.data.type === "messages" &&
        event.data.usage,
    );
    assert.equal(usage.length, 1);
    assert.deepEqual(usage[0].data.usage, {
      model: "test-model",
      prompt_tokens: 15,
      completion_tokens: 5,
      prompt_cached_tokens: 5,
    });
  }
});

for (const name of Object.keys(harnessContracts)) {
  test(`ExoHarness direct contract: ${name}`, async () => {
    const { PROVIDERS } = await mf.getBindings("exo");
    const result = JSON.parse(
      await PROVIDERS.getByName(`contract-${name}`).runContract(name),
    );
    assert.equal(result.ok, true, result.stack ?? result.error);
  });
}

test("ExoHarness direct contract: SQLite, R2, vaults and cursors survive runtime restart", async () => {
  let { PROVIDERS } = await mf.getBindings("exo");
  const checkpoint = await PROVIDERS.getByName(
    "contract-persistence",
  ).seedCheckpoint();
  await mf.dispose();
  mf = new Miniflare(await options());
  await mf.ready;
  ({ PROVIDERS } = await mf.getBindings("exo"));
  await PROVIDERS.getByName("contract-persistence").verifyCheckpoint(
    checkpoint,
  );
});

test("managed thread pagination follows latest activity", async () => {
  const { agent, thread, path } = await create();
  const second = (await api(`agent/${agent.id}/thread`, "POST", {})).thread;
  const third = (await api(`agent/${agent.id}/thread`, "POST", {})).thread;
  await api(`${path}/artifact`, "POST", { path: "touch.txt", contents: [1] });
  const page = await api(`agent/${agent.id}/thread?limit=2`);
  assert.deepEqual(
    page.threads.map((item) => item.id),
    [thread.id, third.id],
  );
  assert.equal(page.next_cursor, page.threads[1].latest_event_id);
  const next = await api(
    `agent/${agent.id}/thread?limit=2&cursor=${page.next_cursor}`,
  );
  assert.deepEqual(
    next.threads.map((item) => item.id),
    [second.id],
  );
  assert.equal(next.next_cursor, null);
});
