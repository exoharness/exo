import assert from "node:assert/strict";
import { readFile, readdir, mkdtemp, mkdir, rm } from "node:fs/promises";
import { resolve } from "node:path";
import { afterEach, beforeEach, test } from "node:test";
import { Miniflare } from "miniflare";
import ts from "typescript";
import { runContracts } from "../scripts/contract-test.mjs";
const fakeCodex = ts.transpileModule(
  await readFile(
    new URL(
      "../../typescript/codex/fixtures/fake-app-server.ts",
      import.meta.url,
    ),
    "utf8",
  ),
  {
    compilerOptions: {
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.ESNext,
    },
  },
).outputText;

const token = "test-operator-token";
let mf;
let persistence;
const key = "synthetic-model-key";
let mcpCalls = 0;
let oauthRefreshes = 0;

const fakeSandbox = await readFile(
  new URL("./fixtures/sandbox.mjs", import.meta.url),
  "utf8",
);

async function options({ accessAud, access, staticToken = token } = {}) {
  const modules = {
    "index.js": {
      type: "esm",
      contents: `import { ExoProvider as Provider, ExoThread as Thread, proxyEgress } from "./implementation.js";
export class ExoProvider extends Provider {
  failNextSubscriptionForTest() { this.failNextSubscription = true; }
  subscribe(...args) { if (this.failNextSubscription) { this.failNextSubscription = false; throw new Error("transient subscription failure"); } return super.subscribe(...args); }
  async proxyForTest(identity, request) { try { const response = await proxyEgress(identity, request, this); return {status: response.status, body: await response.text()}; } catch (error) { return {error: error.message}; } }
}
export class ExoThread extends Thread {
  async harnessRequestForTest(thread_id, request) { try { return await this.runtime.call({type: "harness_request", thread_id, request}); } catch (error) { return {error: error.message}; } }
  alarmForTest() { return this.ctx.storage.getAlarm(); }
  recoverForTest() { return this.alarm(); }
}
export {default, ExoSandbox, ExoEgress} from "./implementation.js";`,
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
    resourcePersistencePath:
      accessAud !== undefined || staticToken !== token
        ? undefined
        : persistence,
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
            THREADS: {
              type: "durable-object",
              worker: "exo",
              exportName: "ExoThread",
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
            ExoThread: { type: "durable-object", storage: "sqlite" },
          },
        },
        dev: {
          access,
          outboundService: {
            type: "fetcher",
            handler: async (request) => {
              if (new URL(request.url).hostname === "auth.example") {
                const body = new URLSearchParams(await request.text());
                assert.equal(body.get("grant_type"), "refresh_token");
                assert.equal(body.get("refresh_token"), "test-refresh");
                oauthRefreshes++;
                return Response.json({
                  access_token: "synthetic-mcp-key",
                  refresh_token: "rotated-refresh",
                  token_type: "Bearer",
                  expires_in: 3600,
                });
              }
              if (new URL(request.url).hostname === "mcp.example") {
                assert.equal(
                  request.headers.get("authorization"),
                  "Bearer synthetic-mcp-key",
                );
                if (request.method === "GET")
                  return new Response(null, { status: 405 });
                const body = await request.json();
                if (body.id === undefined)
                  return new Response(null, { status: 202 });
                let result;
                switch (body.method) {
                  case "initialize":
                    result = {
                      protocolVersion: body.params.protocolVersion,
                      capabilities: { tools: {} },
                      serverInfo: { name: "fixture", version: "1.0" },
                    };
                    break;
                  case "tools/list":
                    result = {
                      tools: [
                        {
                          name: "echo",
                          description: "Echo text",
                          inputSchema: {
                            type: "object",
                            properties: { text: { type: "string" } },
                            required: ["text"],
                          },
                        },
                        { name: "blocked", inputSchema: { type: "object" } },
                      ],
                    };
                    break;
                  case "tools/call":
                    assert.equal(body.params.name, "echo");
                    mcpCalls++;
                    result = {
                      content: [
                        { type: "text", text: body.params.arguments.text },
                      ],
                    };
                    break;
                  default:
                    throw new Error(`unexpected MCP method: ${body.method}`);
                }
                return Response.json({ jsonrpc: "2.0", id: body.id, result });
              }
              if (
                request.headers.get("authorization") ===
                "Bearer revoked-egress-token"
              )
                return new Response(null, { status: 401 });
              if (
                new URL(request.url).hostname !== "model.example" ||
                new URL(request.url).pathname === "/echo"
              )
                return Response.json(
                  {
                    header: request.headers.get("authorization"),
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
                        name: body.tools.some(
                          (t) => t.name === "exo_mcp__fixture__echo",
                        )
                          ? "exo_mcp__fixture__echo"
                          : "shell",
                        arguments: JSON.stringify(
                          body.tools.some(
                            (t) => t.name === "exo_mcp__fixture__echo",
                          )
                            ? { text: "MCP works" }
                            : { command: "uname -s" },
                        ),
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
            modules: {
              "index.js": { type: "esm", contents: fakeSandbox },
              "fake-app-server.js": { type: "esm", contents: fakeCodex },
            },
          },
          exports: {
            FakeSandbox: { type: "durable-object", storage: "sqlite" },
          },
        },
      },
    ],
  };
}

async function withWorker(config, run) {
  const worker = new Miniflare(await options(config));
  try {
    return await run(worker);
  } finally {
    await worker.dispose();
  }
}

async function api(
  path,
  method = "GET",
  body,
  expected = 200,
  worker = mf,
  bearer = token,
) {
  const response = await worker.dispatchFetch(`https://exo.test/exo/${path}`, {
    method,
    headers: {
      ...(bearer ? { authorization: `Bearer ${bearer}` } : {}),
      "content-type": "application/json",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  assert(text, `${method} ${path}: empty body (${response.status})`);
  const value = JSON.parse(text);
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
  `---\nname: Test Agent\nharness: basic\nmodel:\n  name: test-model\n  base_url: https://model.example/v1\n  credential: OPENAI_API_KEY\npermission_policy: {type: ${ask ? "always_ask" : "always_allow"}}\n---\nUse shell to print Linux, then finish.`;
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
const sendTurn = (path, content = "Print Linux.", options = {}) =>
  api(
    `${path}/turn`,
    "POST",
    { input: { role: "user", content }, ...options },
    202,
  );
const waitTurn = (path, id) =>
  waitEvents(path, (events) =>
    events.some(
      (event) =>
        event.data.type === "turn_ended" && (!id || event.turn_id === id),
    ),
  );

async function assertTurnInactive(agent, thread) {
  const { THREADS } = await mf.getBindings("exo");
  const result = await THREADS.getByName(
    JSON.stringify(["test-account", agent.id, thread.id]),
  ).harnessRequestForTest(thread.id, {
    type: "authorize_tool",
    request: { function_name: "shell", arguments: { command: "uname -s" } },
  });
  assert.match(result.error, /TypeScript turn is not active/);
}

async function waitApproval(path, turnId) {
  const matches = (event) =>
    event.data.event_type === "agent_runtime.approval_requested" &&
    (!turnId || event.turn_id === turnId);
  return (await waitEvents(path, (events) => events.some(matches))).find(
    matches,
  ).data.payload;
}

function assertNoErrors(events) {
  assert(
    !events.some((event) => event.data.type === "error"),
    JSON.stringify(events),
  );
}

async function waitUntil(predicate, message) {
  for (const deadline = Date.now() + 30_000; Date.now() < deadline; ) {
    const result = await predicate();
    if (result) return result;
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail(typeof message === "function" ? message() : message);
}

async function waitEvents(path, predicate) {
  let last;
  return waitUntil(
    async () => {
      last = (await api(`${path}/event?limit=1000`)).events;
      return predicate(last) && last;
    },
    () => `timed out waiting for events: ${JSON.stringify(last)}`,
  );
}

beforeEach(async () => {
  mcpCalls = 0;
  oauthRefreshes = 0;
  await mkdir(".local", { recursive: true });
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
afterEach(async () => {
  await mf?.dispose();
  if (persistence) await rm(persistence, { recursive: true, force: true });
});

test("bearer authentication protects reads, mutations, SSE and RPC", async () => {
  for (const [path, method, authorization] of [
    ["agent", "GET", undefined],
    ["agent", "POST", "Bearer wrong-token"],
    [
      `agent/${crypto.randomUUID()}/thread/${crypto.randomUUID()}`,
      "GET",
      undefined,
    ],
    [
      `agent/${crypto.randomUUID()}/thread/${crypto.randomUUID()}/event/watch`,
      "GET",
      "Bearer wrong-token",
    ],
    ["request", "POST", undefined],
  ]) {
    assert.equal(
      (
        await mf.dispatchFetch("https://exo.test/exo/" + path, {
          method,
          headers: authorization ? { authorization } : {},
        })
      ).status,
      401,
    );
  }
  assert.deepEqual(await api("identity"), { account_id: "test-account" });
  await withWorker({ staticToken: null }, async (worker) => {
    assert.equal(
      (await worker.dispatchFetch("https://exo.test/exo/identity")).status,
      401,
    );
  });
});

test("Access validates platform context and forwards authenticated HTTP and SSE", async () => {
  for (const access of [undefined, { aud: "other-app" }]) {
    await withWorker({ accessAud: "exo-app", access }, async (worker) => {
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
      // Bindings expose RPC methods; no account HTTP handler bypasses Access.
      const { PROVIDERS } = await worker.getBindings("exo");
      await assert.rejects(
        PROVIDERS.getByName("test-account").fetch(
          new Request("https://exo.test/exo/identity", {
            headers: { authorization: `Bearer ${token}` },
          }),
        ),
        /does not export a fetch/,
      );
    });
  }
  await withWorker(
    {
      accessAud: "exo-app",
      access: { aud: "exo-app", identity: { email: "operator@example.com" } },
      staticToken: null,
    },
    async (worker) => {
      const accessApi = (path, method = "GET", body) =>
        api(path, method, body, 200, worker, null);
      assert.deepEqual(await accessApi("identity"), {
        account_id: "test-account",
      });
      const agent = await accessApi("agent", "POST", {
        slug: "access-agent",
        name: "Access agent",
      });
      assert.equal((await accessApi("agent")).agents[0].id, agent.id);
      await accessApi(`agent/${agent.id}/artifact`, "POST", {
        path: "managed-agents/agent.md",
        contents: [...new TextEncoder().encode(definition(false))],
      });
      const { thread } = await accessApi(
        `agent/${agent.id}/thread`,
        "POST",
        {},
      );
      const stream = await worker.dispatchFetch(
        `https://exo.test/exo/agent/${agent.id}/thread/${thread.id}/event/watch`,
      );
      assert.equal(stream.status, 200);
      assert.equal(
        stream.headers.get("content-type"),
        "text/event-stream; charset=utf-8",
      );
      const reader = stream.body.getReader();
      try {
        const { value, done } = await reader.read();
        assert.equal(done, false);
        assert.match(new TextDecoder().decode(value), /event: exo_event/);
      } finally {
        await reader.cancel();
      }
    },
  );
});

test("managed basic turn calls the model, executes one tool and persists canonical events", async () => {
  const { agent, thread, path } = await create();
  assert.equal(await api(`${path}/previews`), null);
  await api(
    `agent/${agent.id}/thread/${crypto.randomUUID()}/previews`,
    "GET",
    undefined,
    404,
  );
  const submitted = await sendTurn(path);
  const events = await waitTurn(path);
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
  assert.equal(await (await sandboxFor(agent, thread)).count(), 1);
});

test("approval pauses the turn and executes only after approval", async () => {
  const { agent, thread, path } = await create(true);
  const { ARTIFACTS } = await mf.getBindings("exo");
  const blobsBefore = (await ARTIFACTS.list()).objects.length;
  const submitted = await sendTurn(path);
  const approval = await waitApproval(path, submitted.turn.id);
  assert.equal(
    (await ARTIFACTS.list()).objects.length,
    blobsBefore,
    "the empty unfinished-turn marker must stay in Durable Object storage",
  );
  assert.equal(await (await sandboxFor(agent, thread)).count(), 0);
  await api(`${path}/turn/${submitted.turn.id}/approval-response`, "POST", {
    session_id: submitted.turn.session_id,
    approval_id: approval.approval_id,
    approved: true,
  });
  await waitTurn(path);
  assert.equal(await (await sandboxFor(agent, thread)).count(), 1);
});

test("artifact versions, event cursors and encrypted vaults survive a runtime restart", async () => {
  const contents = Array.from({ length: 70 * 1024 }, (_, i) => i % 256);
  const inlineContents = Array.from({ length: 64 * 1024 }, (_, i) => i % 256);
  const { agent, path } = await create();
  const { ARTIFACTS } = await mf.getBindings("exo");
  const blobsBefore = (await ARTIFACTS.list()).objects.length;
  const first = await writeThreadArtifact(agent.id, path.split("/")[3], {
    path: "result.bin",
    contents,
  });
  const second = await writeThreadArtifact(agent.id, path.split("/")[3], {
    path: "result.bin",
    contents: inlineContents,
  });
  assert.equal(
    (await ARTIFACTS.list()).objects.length,
    blobsBefore + 1,
    "only the artifact over 64 KiB belongs in R2",
  );
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
    contents,
  );
  assert.deepEqual(
    (
      await api(
        `${path}/artifact/read?artifact_id=${second.artifact_id}&version=2`,
      )
    ).contents,
    inlineContents,
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
  await sendTurn(path);
  await waitTurn(path);
});

test("queued cancellation and idempotency are isolated from another thread's execution", async () => {
  const { path } = await create(true);
  const first = await sendTurn(path);
  await waitApproval(path);
  const queued = await sendTurn(path, "queued", {
    idempotency_key: "queued-key",
  });
  const alias = path
    .split("/")
    .map((part, index) => (index % 2 ? part.toUpperCase() : part))
    .join("/");
  const duplicate = await sendTurn(alias, "duplicate", {
    idempotency_key: "queued-key",
  });
  assert.deepEqual(duplicate.turn, queued.turn);
  const independent = await create();
  const other = await sendTurn(independent.path);
  await waitTurn(independent.path, other.turn.id);
  assert.deepEqual(await api(`${path}/turn/${first.turn.id}`), {
    active: true,
  });
  assert.equal(
    (await api(`${path}/turn/${queued.turn.id}/cancel`, "POST", {}))
      .canceled_active_turn,
    true,
  );
  await api(`${path}/turn/${first.turn.id}/cancel`, "POST", {});
  const events = await waitTurn(path, queued.turn.id);
  assert(
    !events.some(
      (event) =>
        event.turn_id === queued.turn.id &&
        event.data.event_type === "agent_runtime.approval_requested",
    ),
  );
  assert.deepEqual(
    (
      await sendTurn(path, "completed duplicate", {
        idempotency_key: "queued-key",
      })
    ).turn,
    queued.turn,
  );
});

test("interrupt controls the running head through the shared coordinator", async () => {
  const { path } = await create(true);
  const first = await sendTurn(path);
  await waitApproval(path);
  const next = await sendTurn(path, "interrupt", { attention: "interrupt" });
  await waitTurn(path, first.turn.id);
  await waitApproval(path, next.turn.id);
  await api(`${path}/turn/${next.turn.id}/cancel`, "POST", {});
  await waitTurn(path, next.turn.id);
});

test("Worker restart preserves the thread queue and accepted turn identities", async () => {
  const { path } = await create(true);
  const first = await sendTurn(path);
  await waitApproval(path);
  const queued = await sendTurn(path, "queued after restart", {
    idempotency_key: "restart-key",
  });
  const { THREADS } = await mf.getBindings("exo");
  const name = JSON.stringify([
    "test-account",
    path.split("/")[1],
    path.split("/")[3],
  ]);
  assert(
    (await THREADS.getByName(name).alarmForTest()) > Date.now(),
    "acceptance persists a recovery alarm",
  );
  assert.deepEqual(await api(`${path}/turn/${queued.turn.id}`), {
    active: false,
  });

  await mf.dispose();
  mf = new Miniflare(await options());
  await mf.ready;
  assert.deepEqual(
    (
      await sendTurn(path, "duplicate after restart", {
        idempotency_key: "restart-key",
      })
    ).turn,
    queued.turn,
  );
  await api(`${path}/turn/${first.turn.id}/cancel`, "POST", {});
  await waitApproval(path, queued.turn.id);
  await api(`${path}/turn/${queued.turn.id}/cancel`, "POST", {});
  const events = await waitTurn(path, queued.turn.id);
  assert.equal(
    events.filter((event) => event.data.type === "turn_started").length,
    2,
  );
  assert.equal(
    events.filter(
      (event) =>
        event.turn_id === queued.turn.id && event.data.type === "turn_started",
    ).length,
    1,
  );
  const restored = await mf.getBindings("exo");
  await waitUntil(
    () =>
      restored.THREADS.getByName(name)
        .alarmForTest()
        .then((alarm) => alarm === null),
    "queue alarm was not cleared",
  );
});

async function watch(path, after) {
  const abort = new AbortController();
  const response = await mf.dispatchFetch(
    `https://exo.test/exo/${path}/event/watch${after ? `?after=${after}` : ""}`,
    { headers: { authorization: `Bearer ${token}` }, signal: abort.signal },
  );
  assert.equal(
    response.headers.get("content-type"),
    "text/event-stream; charset=utf-8",
  );
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
  return { received, collect, cancel: () => abort.abort() };
}

test("Codex reuses its RPC process across turns and resumes after backend shutdown", async () => {
  const { agent, thread, path } = await create(false, "codex");
  const cursor = (await api(`${path}/event`)).events[0].id;
  const streaming = await watch(path, cursor);
  const first = await sendTurn(path, "run tests");
  assert.equal(first.harness, "codex");
  const events = await waitTurn(path);
  await assertTurnInactive(agent, thread);
  assert.match(
    (await sandboxRecord(agent, thread)).image,
    /^codex-[0-9a-f]{8}$/,
  );
  assertNoErrors(events);
  assert(
    events.some(
      (event) =>
        event.data.type === "tool_result" && event.data.result.exit_code === 0,
    ),
  );
  assert(
    JSON.stringify(events).includes("Done. 🧪"),
    "process chunks must preserve split UTF-8",
  );
  await streaming.collect;
  streaming.cancel();
  assert(!streaming.received.some((event) => event.id === cursor));
  assert.equal(
    new Set(streaming.received.map((event) => event.id)).size,
    streaming.received.length,
  );
  const processEvent = events.find(
    (event) =>
      event.data.type === "sandbox_process_event" &&
      ["stdout", "stderr"].includes(event.data.event.type),
  );
  assert(processEvent, "process byte events must be persisted");
  assert.deepEqual(
    streaming.received.find((event) => event.id === processEvent.id),
    processEvent,
    "SSE byte fields must match the REST JSON representation",
  );
  assert(
    streaming.received.some(
      (event) => event.data.type === "lingua_stream_chunk",
    ),
  );
  assert(!events.some((event) => event.data.type === "lingua_stream_chunk"));
  const sandbox = await sandboxFor(agent, thread);
  assert.equal(await sandbox.activityCount(), 0);
  assert.equal(await sandbox.lastMethod(), "thread/start");

  const second = await sendTurn(path, "continue");
  const finished = await waitTurn(path, second.turn.id);
  assertNoErrors(finished);
  assert.equal(await sandbox.lastMethod(), "thread/start");
  const started = finished.find(
    (event) =>
      event.turn_id === second.turn.id &&
      event.data.event_type === "codex_turn_started",
  );
  assert.equal(started.data.payload.hydrated_from, "warm_codex_thread");
  assert.equal(started.data.payload.warm_app_server_reused, true);
  assert.equal(started.data.payload.warm_thread_reused, true);
  assert.equal(await sandbox.processCount(), 1);
  const managed = finished.filter(
    (event) =>
      event.data.type === "sandbox_process_started" &&
      event.data.command.some((arg) => arg.includes("app-server")),
  );
  assert.equal(
    managed.length,
    1,
    "warm Codex must reuse the manager's process",
  );
  const scope = { type: "thread", agent_id: agent.id, thread_id: thread.id };
  const tracked = await rpc({
    type: "get_sandbox_process_events",
    scope,
    query: {
      sandbox_id: managed[0].data.sandbox_id,
      process_id: managed[0].data.process_id,
      after: null,
      limit: 0,
      follow: false,
    },
  });
  assert.equal(tracked.result.status.type, "running");
  await sandbox.stop();
  assert.equal(await sandbox.processCount(), 0);
  const third = await sendTurn(path, "resume after idle");
  const resumed = await waitTurn(path, third.turn.id);
  assertNoErrors(resumed);
  assert.equal(
    resumed.filter(
      (event) =>
        event.data.type === "sandbox_process_started" &&
        event.data.command.some((arg) => arg.includes("app-server")),
    ).length,
    2,
  );
  const resumedStart = resumed.find(
    (event) =>
      event.turn_id === third.turn.id &&
      event.data.event_type === "codex_turn_started",
  );
  assert.equal(resumedStart.data.payload.warm_app_server_reused, false);
  assert.equal(resumedStart.data.payload.hydrated_from, "warm_codex_thread");
  assert.equal(await sandbox.lastMethod(), "thread/resume");
  assert.equal(await sandbox.processCount(), 1);
  await sandbox.stop();
  for (const harness of ["basic", "codex"]) {
    await api("agent/" + agent.id + "/artifact", "POST", {
      path: "managed-agents/agent.md",
      contents: [
        ...new TextEncoder().encode(
          definition(false).replace("harness: basic", "harness: " + harness),
        ),
      ],
    });
    const switched = await sendTurn(path);
    const switchedEvents = await waitTurn(path, switched.turn.id);
    assertNoErrors(switchedEvents);
  }
  assert.equal(
    await sandbox.processCount(),
    1,
    "switching harnesses must discard the old JS session",
  );
  await sandbox.stop();
});

test(
  "Codex cancellation uses the shared turn lifecycle and stops its managed process",
  { timeout: 60_000 },
  async () => {
    const { agent, thread, path } = await create(false, "codex");
    const submitted = await sendTurn(path, "hold-turn");
    const events = await waitEvents(path, (events) =>
      events.some((event) => event.data.event_type === "codex_turn_started"),
    );
    assert(!events.some((event) => event.data.type === "turn_ended"));
    const sandbox = await sandboxFor(agent, thread);
    assert.equal(await sandbox.activityCount(), 1);
    const process = events.find(
      (event) =>
        event.data.type === "sandbox_process_started" &&
        event.data.command.some((arg) => arg.includes("app-server")),
    ).data;
    assert.equal(
      (await api(`${path}/turn/${submitted.turn.id}/cancel`, "POST", {}))
        .canceled_active_turn,
      true,
    );
    await waitTurn(path);
    await assertTurnInactive(agent, thread);
    assert.equal(await sandbox.activityCount(), 0);
    const result = await rpc({
      type: "get_sandbox_process_events",
      scope: {
        type: "thread",
        agent_id: agent.id,
        thread_id: thread.id,
      },
      query: {
        sandbox_id: process.sandbox_id,
        process_id: process.process_id,
        after: null,
        limit: 0,
        follow: false,
      },
    });
    assert.notEqual(result.result.status.type, "running");
    assert.equal(await (await sandboxFor(agent, thread)).processCount(), 0);
    // Cancel while the backend has created a process but has not returned its handle.
    await sandbox.blockNextStart();
    const starting = await sendTurn(path, "hold-turn");
    await sandbox.waitForBlockedStart();
    await api(`${path}/turn/${starting.turn.id}/cancel`, "POST", {});
    await sandbox.releaseBlockedStart();
    await waitTurn(path, starting.turn.id);
    await waitUntil(
      async () => (await sandbox.processCount()) === 0,
      "abandoned process did not close",
    );
    assert.equal(
      await sandbox.processCount(),
      0,
      "an abandoned start must close its returned process",
    );
  },
);

async function rpc(request) {
  const response = await api("request", "POST", {
    kind: "request",
    id: 1,
    request,
  });
  assert.equal(response.ok, true, response.error);
  return response.response;
}
async function sandboxRecord(agent, thread) {
  const result = await rpc({
    type: "list_sandboxes",
    scope: { type: "thread", agent_id: agent.id, thread_id: thread.id },
  });
  assert(result.sandboxes.length > 0);
  assert.notEqual(result.sandboxes[0].id, thread.id);
  return result.sandboxes[0];
}
async function sandboxFor(agent, thread) {
  const { SANDBOXES } = await mf.getBindings("exo");
  return SANDBOXES.getByName((await sandboxRecord(agent, thread)).id);
}
async function writeThreadArtifact(agent_id, conversation_id, request) {
  return (
    await rpc({
      type: "conversation_write_artifact",
      agent_id,
      conversation_id,
      request,
    })
  ).artifact;
}

test("sandbox process RPC preserves binary stdin, drains output and cancels processes", async () => {
  const { agent, thread } = await create();
  const scope = { type: "thread", agent_id: agent.id, thread_id: thread.id };
  const { sandbox_id } = await rpc({
    type: "create_sandbox",
    scope,
    request: {
      name: "io-test",
      resources: { vcpu_count: 2, memory_mib: 4096 },
      provider: "cloudflare",
      image: "",
      default_workdir: "/workspace",
      file_system_mounts: null,
      durable_file_systems: null,
      enable_networking: false,
      idle_seconds: 60,
    },
  });
  const start = async () =>
    (
      await rpc({
        type: "start_sandbox_process",
        scope,
        request: { sandbox_id, command: ["cat"], cwd: null, stdin: "open" },
      })
    ).process.id;
  const process_id = await start();
  const sandbox = await sandboxFor(agent, thread);
  assert.deepEqual(JSON.parse(await sandbox.resources()), {
    vcpu_count: 2,
    memory_mib: 4096,
  });
  const followed = rpc({
    type: "get_sandbox_process_events",
    scope,
    query: {
      sandbox_id,
      process_id,
      after: null,
      limit: null,
      follow: true,
    },
  });
  const data = Array.from({ length: 32768 }, (_, i) => i % 256);
  await rpc({
    type: "write_sandbox_process_input",
    scope,
    request: { sandbox_id, process_id, data },
  });
  assert(
    (await followed).result.events.some((event) => event.type === "stdout"),
  );
  await rpc({
    type: "close_sandbox_process_input",
    scope,
    request: { sandbox_id, process_id },
  });
  const status = await rpc({
    type: "wait_sandbox_process",
    scope,
    request: { sandbox_id, process_id },
  });
  assert.deepEqual(status.status, { type: "exited", exit_code: 0 });
  const output = await rpc({
    type: "get_sandbox_process_events",
    scope,
    query: { sandbox_id, process_id, after: null, limit: null, follow: false },
  });
  assert.deepEqual(
    output.result.events
      .filter((e) => e.type === "stdout")
      .flatMap((e) => e.data),
    data,
  );
  const second = await start();
  const canceled = await rpc({
    type: "cancel_sandbox_process",
    scope,
    request: { sandbox_id, process_id: second, signal: null },
  });
  assert.deepEqual(canceled.status, { type: "cancelled" });
  const { SANDBOXES } = await mf.getBindings("exo");
  await waitUntil(
    async () => (await SANDBOXES.getByName(sandbox_id).processCount()) === 0,
    "cancelled process did not close",
  );
  assert.equal(await SANDBOXES.getByName(sandbox_id).processCount(), 0);
  await mf.dispose();
  mf = new Miniflare(await options());
  const restored = await rpc({
    type: "get_sandbox_process_events",
    scope,
    query: { sandbox_id, process_id, after: null, limit: null, follow: true },
  });
  assert.deepEqual(restored, output);
  const restoredStatus = await rpc({
    type: "wait_sandbox_process",
    scope,
    request: { sandbox_id, process_id },
  });
  assert.deepEqual(restoredStatus, status);
});

test("existing Rust trait contracts run against the Worker store", async () => {
  const endpoint = new URL("/exo", await mf.ready).href;
  await runContracts(endpoint, token);
});

test("Worker egress substitutes credentials and enforces destination policies", async () => {
  const { agent, thread, path } = await create(false, "codex");
  await sendTurn(path);
  await waitTurn(path);
  const sandbox = await sandboxFor(agent, thread);
  const { OPENAI_API_KEY } = await sandbox.environment();
  const { PROVIDERS } = await mf.getBindings("exo");
  const provider = PROVIDERS.getByName("test-account");
  const identity = {
    agentId: agent.id,
    threadId: thread.id,
    sandboxId: (await sandboxRecord(agent, thread)).id,
  };
  const headers = { authorization: `Bearer ${OPENAI_API_KEY}` };
  const proxy = (identity, url, headers) =>
    provider.proxyForTest(identity, new Request(url, { headers }));
  const denied = async (identity, url, message, headers) =>
    assert.equal((await proxy(identity, url, headers)).status, 403, url);
  for (const [url, message] of [
    ["https://other.example/", /credential placeholder does not match/],
    ["http://model.example/echo", /credential substitution requires HTTPS/],
  ])
    await denied(identity, url, message, headers);
  for (const host of [
    "127.0.0.1",
    "[::1]",
    "localhost",
    "app.localhost",
    "app.internal",
    "app.local",
  ])
    await denied(identity, `https://${host}/`, /public DNS hostname/);
  for (const url of [
    "https://model.example:8443/echo",
    "http://model.example:8080/echo",
    "http://model.example:443/echo",
    "https://model.example:80/echo",
  ])
    await denied(identity, url, /HTTP port 80 and HTTPS port 443/);
  const response = await proxy(identity, "https://model.example/echo", headers);
  assert.equal(response.status, 302);
  assert.deepEqual(JSON.parse(response.body), {
    header: `Bearer ${key}`,
  });
  assert.equal(
    (await proxy(identity, "http://model.example/echo")).status,
    302,
  );
  for (const networking of [
    { type: "limited", allowed_hosts: ["MODEL.EXAMPLE"] },
    { type: "disabled" },
  ]) {
    const scope = { type: "thread", agent_id: agent.id, thread_id: thread.id };
    const { sandbox_id } = await rpc({
      type: "create_sandbox",
      scope,
      request: {
        provider: "cloudflare",
        image: "",
        idle_seconds: 60,
        policy: { networking },
      },
    });
    const restricted = { ...identity, sandboxId: sandbox_id };
    await denied(
      restricted,
      "https://other.example/",
      /network destination denied/,
    );
    if (networking.type === "limited")
      assert.equal(
        (await proxy(restricted, "https://model.example/echo")).status,
        302,
      );
    else
      await denied(
        restricted,
        "https://model.example/echo",
        /network destination denied/,
      );
    await rpc({ type: "stop_sandbox", scope, sandbox_id });
  }
  await denied(identity, "https://model.example/echo", /upgrade/, {
    upgrade: "websocket",
  });
  assert.equal(
    (await proxy(identity, "https://model.example/echo", { "x-latin": "café" }))
      .status,
    302,
  );
  const vault = (await api("vault")).find((vault) => vault.name === "global");
  await api(`vault/${vault.id}/secret`, "POST", {
    name: "egress-oauth",
    policy: policy("https://model.example"),
    secret: {
      type: "oauth",
      access_token: "revoked-egress-token",
      refresh_token: "test-refresh",
      expires_at: 4000000000,
      refresh: {
        token_endpoint: "https://auth.example/token",
        client_id: "test-client",
        scopes: [],
        resource: null,
      },
    },
  });
  const scope = { type: "thread", agent_id: agent.id, thread_id: thread.id };
  const { sandbox_id } = await rpc({
    type: "create_sandbox",
    scope,
    request: {
      provider: "cloudflare",
      image: "",
      idle_seconds: 60,
      policy: {
        networking: { type: "unrestricted" },
        credentials: [
          {
            name: "egress-oauth",
            environment_variable: "OAUTH",
            ...policy("https://model.example"),
          },
        ],
      },
    },
  });
  const { SANDBOXES } = await mf.getBindings("exo");
  const { OAUTH } = await SANDBOXES.getByName(sandbox_id).environment();
  const metadata = await provider.proxyHeaders(
    { ...identity, sandboxId: sandbox_id },
    {
      url: "https://model.example/echo",
      method: "GET",
      headers: [["authorization", "Bearer " + OAUTH]],
    },
  );
  assert.match(metadata.credentials[0][1], /^[a-f0-9]{64}$/);
  assert(
    !JSON.stringify(metadata.credentials).includes("revoked-egress-token"),
  );
  const beforeUpload = oauthRefreshes;
  const upload = await provider.proxyForTest(
    { ...identity, sandboxId: sandbox_id },
    new Request("https://model.example/echo", {
      method: "POST",
      headers: { authorization: "Bearer " + OAUTH },
      body: "upload",
    }),
  );
  assert.equal(
    upload.status,
    401,
    "request bodies are sent once without replay",
  );
  assert.equal(oauthRefreshes, beforeUpload);
  const refreshes = oauthRefreshes;
  const refreshed = await proxy(
    { ...identity, sandboxId: sandbox_id },
    "https://model.example/echo",
    { authorization: `Bearer ${OAUTH}` },
  );
  assert.equal(refreshed.status, 302);
  assert.equal(JSON.parse(refreshed.body).header, "Bearer synthetic-mcp-key");
  assert.equal(oauthRefreshes, refreshes + 1);
  await rpc({ type: "stop_sandbox", scope, sandbox_id });
  await sandbox.stop();
});

test("MCP uses vault OAuth refresh, shared filtering and basic tool execution", async () => {
  const vault = await api("vault", "POST", { name: "mcp-test" });
  await api(`vault/${vault.id}/secret`, "POST", {
    name: "fixture",
    policy: policy("https://mcp.example"),
    secret: {
      type: "oauth",
      access_token: "expired-token",
      refresh_token: "test-refresh",
      expires_at: 0,
      refresh: {
        token_endpoint: "https://auth.example/token",
        client_id: "test-client",
        scopes: [],
        resource: null,
      },
    },
  });
  const agent = await api("agent", "POST", {
    slug: `mcp-${crypto.randomUUID()}`,
    name: "MCP test",
  });
  const source = definition(false).replace(
    "permission_policy:",
    "mcp_servers:\n  - type: url\n    name: fixture\n    url: https://mcp.example/mcp\n    allowed_tools: [echo]\n    permission_policy: {type: always_allow}\npermission_policy:",
  );
  await api(`agent/${agent.id}/artifact`, "POST", {
    path: "managed-agents/agent.md",
    contents: [...new TextEncoder().encode(source)],
  });
  const before = { calls: mcpCalls, refreshes: oauthRefreshes };
  const { thread } = await api(`agent/${agent.id}/thread`, "POST", {
    vaults: [vault.id],
  });
  const path = `agent/${agent.id}/thread/${thread.id}`;
  const turn = await sendTurn(path);
  const events = await waitTurn(path, turn.turn.id);
  assert(!events.some((e) => e.data.type === "error"), JSON.stringify(events));
  const tool = events.find(
    (e) => e.data.type === "tool_result" && e.data.result?.content,
  );
  assert.equal(tool.data.result.content[0].text, "MCP works");
  assert.equal(mcpCalls, before.calls + 1);
  assert.equal(oauthRefreshes, before.refreshes + 1);
  const inventory = events.find((e) => e.data.event_type === "mcp_tools").data
    .payload;
  assert.deepEqual(
    inventory.map((t) => t.name),
    ["exo_mcp__fixture__echo"],
  );
});

test("Codex receives MCP configuration and Git resources through shared sandbox policy", async () => {
  const vault = await api("vault", "POST", { name: "codex-resources-test" });
  await api(`vault/${vault.id}/secret`, "POST", {
    name: "fixture",
    secret: { type: "key", value: "synthetic-mcp-key" },
    policy: policy("https://mcp.example"),
  });
  await api(`vault/${vault.id}/secret`, "POST", {
    name: "git",
    secret: { type: "key", value: "synthetic-git-key" },
    policy: policy("https://github.com"),
  });
  const agent = await api("agent", "POST", {
    slug: `codex-resources-${crypto.randomUUID()}`,
    name: "Codex resources",
  });
  const source = definition(false)
    .replace("harness: basic", "harness: codex")
    .replace(
      "permission_policy:",
      "resources:\n  - name: repo\n    type: git\n    url: https://github.com/example/project.git\n    credential: git\n    mount_path: /workspace\nmcp_servers:\n  - type: url\n    name: fixture\n    url: https://mcp.example/mcp\n    blocked_tools: [blocked]\n    permission_policy: {type: always_allow}\npermission_policy:",
    );
  await api(`agent/${agent.id}/artifact`, "POST", {
    path: "managed-agents/agent.md",
    contents: [...new TextEncoder().encode(source)],
  });
  const { thread } = await api(`agent/${agent.id}/thread`, "POST", {
    vaults: [vault.id],
  });
  const path = `agent/${agent.id}/thread/${thread.id}`;
  const live = await watch(path);
  const turn = await sendTurn(path);
  const events = await waitTurn(path, turn.turn.id);
  await live.collect;
  const statuses = live.received
    .filter((event) => event.data.event_type === "runtime_status")
    .map((event) => event.data.payload.message);
  assert(
    statuses.includes("Preparing Git resource github.com/example/project"),
  );
  assert(statuses.includes("Downloading github.com/example/project: 100%"));
  assert(!events.some((e) => e.data.type === "error"), JSON.stringify(events));
  const { sandboxes } = await api("request", "POST", {
    kind: "request",
    id: 1,
    request: {
      type: "list_sandboxes",
      scope: { type: "thread", agent_id: agent.id, thread_id: thread.id },
    },
  }).then((r) => r.response);
  const { SANDBOXES } = await mf.getBindings("exo");
  const environment = await SANDBOXES.getByName(sandboxes[0].id).environment();
  assert.match(environment.EXO_GIT_7265706f, /^exo_egress_/);
  assert(!JSON.stringify(environment).includes("synthetic-git-key"));
  const { fixture } = await SANDBOXES.getByName(sandboxes[0].id).mcpConfig();
  assert.equal(fixture.url, "https://mcp.example/mcp");
  assert.deepEqual(fixture.enabled_tools, ["echo"]);
  assert.match(environment[fixture.bearer_token_env_var], /^exo_egress_/);

  await api(`agent/${agent.id}/artifact`, "POST", {
    path: "managed-agents/agent.md",
    contents: [
      ...new TextEncoder().encode(
        source.replace(
          "github.com/example/project.git",
          "github.com/example/disk-full.git",
        ),
      ),
    ],
  });
  const failed = await api(`agent/${agent.id}/thread`, "POST", {
    vaults: [vault.id],
  });
  const failedPath = `agent/${agent.id}/thread/${failed.thread.id}`;
  const failedLive = await watch(failedPath);
  await sendTurn(failedPath);
  const failedEvents = await waitTurn(failedPath);
  await failedLive.collect;
  assert(
    failedEvents.some(
      (event) =>
        event.data.type === "error" &&
        event.data.message.includes("No space left on device"),
    ),
  );
  assert(
    failedLive.received.some(
      (event) =>
        event.data.type === "error" &&
        event.data.message.includes("No space left on device"),
    ),
  );
});

test("a thread alarm retries a queue drain after a transient host failure", async () => {
  const { agent, thread, path } = await create();
  const { PROVIDERS, THREADS } = await mf.getBindings("exo");
  await PROVIDERS.getByName("test-account").failNextSubscriptionForTest();
  const submitted = await sendTurn(path);
  const owner = THREADS.getByName(
    JSON.stringify(["test-account", agent.id, thread.id]),
  );
  const events = await waitUntil(async () => {
    await owner.recoverForTest();
    const { events } = await api(`${path}/event?limit=1000`);
    return (
      events.some(
        (event) =>
          event.turn_id === submitted.turn.id &&
          event.data.type === "turn_ended",
      ) && events
    );
  }, "alarm did not retry the retained turn");
  assertNoErrors(events);
});
