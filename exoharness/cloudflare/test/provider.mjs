import assert from "node:assert/strict";
import { readFile, readdir, mkdtemp, rm } from "node:fs/promises";
import { resolve } from "node:path";
import { after, before, test } from "node:test";
import { Miniflare } from "miniflare";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import ts from "typescript";
const fakeCodex = ts
  .transpileModule(
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
  )
  .outputText.replace("export class", "class")
  .replace("export {};", "");

const token = "test-operator-token";
let mf;
let persistence;
const key = "synthetic-model-key";
let modelRequests = 0;

const fakeSandbox = `import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";
${fakeCodex}
class FakeProcess extends RpcTarget {
  constructor(process, suppress = () => false) {
    super(); this.process = process;
    this.out = process.stdout.pipeThrough(new TransformStream({transform(line, controller) {
      if (!suppress(line)) controller.enqueue(line);
    }})).pipeThrough(new TextEncoderStream()).pipeThrough(new TransformStream({transform(bytes, controller) {
      const index = bytes.findIndex(byte => byte >= 128);
      if (index < 0) controller.enqueue(bytes);
      else { controller.enqueue(bytes.slice(0, index + 1)); controller.enqueue(bytes.slice(index + 1)); }
    }}));
    this.err = process.stderr.pipeThrough(new TextEncoderStream());
  }
  get stdout() { return this.out; }
  get stderr() { return this.err; }
  get sandboxProcessId() { return "fixture-process"; }
  async writeStdin(bytes) { await this.process.writeStdin(new TextDecoder().decode(bytes)); }
  async closeStdin() { await this.process.closeStdin(); }
  async close() { await this.process.close(); }
  async wait() { return this.process.wait(); }
}
class EchoProcess extends RpcTarget {
  constructor() {
    super();
    this.output = new ReadableStream({type: "bytes", start: c => {this.controller = c;}});
    this.exited = new Promise(resolve => {this.exit = resolve;});
    this.closed = false;
  }
  get stdout() {return this.output;}
  get stderr() {return new ReadableStream({type: "bytes", start: c => c.close()});}
  get sandboxProcessId() {return "echo-process";}
  async writeStdin(bytes) {this.controller.enqueue(bytes);}
  async closeStdin() {if (!this.closed) {this.closed = true; this.controller.close(); this.exit(0);}}
  async close() {await this.closeStdin();}
  async wait() {return this.exited;}
}
export class FakeSandbox extends DurableObject {
  constructor(ctx, env) { super(ctx, env); this.processes = new Set(); this.activities = new Set(); }
  async beginActivity(identity, id) { this.activities.add(id); await this.ctx.storage.put("identity", identity); await this.ctx.storage.deleteAlarm(); }
  async endActivity(id) { this.activities.delete(id); }
  async activityCount() { return this.activities.size; }
  async info() { return {exists: !!await this.ctx.storage.get("identity"), running: await this.ctx.storage.get("running") ?? false}; }
  async acquire(identity, image, cwd, environment) { image ||= "cloudflare/debian-trixie"; await this.ctx.storage.put("identity", identity); await this.ctx.storage.put("image", image); await this.ctx.storage.put("environment", environment); await this.ctx.storage.put("running", true); return image; }
  async terminate() { await this.stop(); await this.ctx.storage.deleteAll(); }
  async exec(identity, request) {
    const count = (await this.ctx.storage.get("count") ?? 0) + 1;
    await this.ctx.storage.put("count", count);
    return {stdout: "Linux test\\n", stderr: "", exitCode: 0};
  }
  async count() { return await this.ctx.storage.get("count") ?? 0; }
  async environment() { return await this.ctx.storage.get("environment"); }
  async startProcess(request) {
    if (request.command[0] === "cat") return new EchoProcess();
    if (!request.command.join(" ").includes("codex")) {
      if (request.command[2] !== "true") await this.ctx.storage.put("count", (await this.count()) + 1);
      return new FakeProcess({stdout: new ReadableStream({start(c) { c.enqueue("Linux test\\n"); c.close(); }}), stderr: new ReadableStream({start(c) {c.close();}}), writeStdin: async () => {}, closeStdin: async () => {}, close: async () => {}, wait: async () => 0});
    }
    let holdTurn = false;
    const app = new FakeCodexAppServer({resumeAvailable: true,
      completedItems: id => [
        {id: "cmd-" + id, type: "commandExecution", command: "node --test", cwd: "/workspace", status: "completed", exitCode: 0, aggregatedOutput: "passed", durationMs: 5},
        {id: "msg-" + id, type: "agentMessage", text: "Done. 🧪"},
      ],
      onTurn: async (threadId, turnId, emit) => {
        emit({method: "item/agentMessage/delta", params: {threadId, turnId, itemId: "msg-" + turnId, delta: "Done. 🧪"}});
      },
    });
    const write = app.process.writeStdin;
    app.process.writeStdin = async line => {const request = JSON.parse(line); if (request.method === "turn/start") holdTurn = JSON.stringify(request.params).includes("hold-turn"); if (["thread/start", "thread/resume"].includes(request.method)) await this.ctx.storage.put("last-method", request.method); await write(line);};
    return new FakeProcess(app.process, line => holdTurn && JSON.parse(line).method === "turn/completed");
  }
  async runProcess(identity, request, ready) {
    const process = await this.startProcess(request); this.processes.add(process);
    const capability = new RpcStub(process);
    try { await ready(capability); await process.wait(); }
    finally { capability[Symbol.dispose](); this.processes.delete(process); }
  }
  async waitForProcesses() { await Promise.all([...this.processes].map(p => p.wait())); }
  async snapshot() { return {id: "fixture-snapshot", size: 1}; }
  async processCount() { return this.processes.size; }
  async lastMethod() { return await this.ctx.storage.get("last-method"); }
  async stop() { await Promise.all([...this.processes].map(p => p.close())); await this.ctx.storage.put("running", false); }

}
export default { fetch() { return new Response("fixture"); } };`;

async function options({ accessAud, access, staticToken = token } = {}) {
  const modules = {
    "index.js": {
      type: "esm",
      contents: `import { ExoProvider as Provider } from "./implementation.js";
export class ExoProvider extends Provider {
  async harnessRequestForTest(thread_id, request) { try { return await this.runtime.call({type: "harness_request", thread_id, request}); } catch (error) { return {error: error.message}; } }
  // Capture the Rust error before crossing Miniflare's RPC bridge.
  async proxyForTest(identity, request) { try { const response = await this.proxy(identity, request); return {status: response.status, body: await response.text()}; } catch (error) { return {error: error.message}; } }
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
const sendTurn = (path, content = "Print Linux.") =>
  api(`${path}/turn`, "POST", { input: { role: "user", content } }, 202);
const waitTurn = (path, id) =>
  waitEvents(path, (events) =>
    events.some(
      (event) =>
        event.data.type === "turn_ended" && (!id || event.turn_id === id),
    ),
  );

async function assertTurnInactive(threadId) {
  const { PROVIDERS } = await mf.getBindings("exo");
  const result = await PROVIDERS.getByName(
    "test-account",
  ).harnessRequestForTest(threadId, {
    type: "authorize_tool",
    request: { function_name: "shell", arguments: { command: "uname -s" } },
  });
  assert.match(result.error, /TypeScript turn is not active/);
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

test("bearer authentication protects reads, mutations, SSE and RPC", async () => {
  for (const [path, method] of [
    ["identity", "GET"],
    ["agent", "POST"],
    ["vault", "POST"],
    ["agent/a/thread/t/event/watch", "GET"],
    ["agent/a/thread/t/turn", "POST"],
    ["agent/a/thread/t/turn/u/cancel", "POST"],
    ["request", "POST"],
  ]) {
    for (const authorization of [
      undefined,
      "Bearer wrong-token",
      "Basic test-operator-token",
      "Bearer ",
    ]) {
      assert.equal(
        (
          await mf.dispatchFetch(`https://exo.test/exo/${path}`, {
            method,
            headers: authorization ? { authorization } : {},
          })
        ).status,
        401,
      );
    }
  }
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
    const { thread } = await accessApi(`agent/${agent.id}/thread`, "POST", {});
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
  const { agent, thread, path } = await create();
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
  assert.equal(modelRequests, 2);
  for (let i = 1; i < events.length; i++)
    assert(events[i - 1].id < events[i].id);
});

test("approval pauses durably, rejects the wrong session and executes only after approval", async () => {
  const { agent, thread, path } = await create(true);
  const { ARTIFACTS } = await mf.getBindings("exo");
  const blobsBefore = (await ARTIFACTS.list()).objects.length;
  const submitted = await sendTurn(path);
  const events = await waitEvents(path, (events) =>
    events.some(
      (event) => event.data.event_type === "agent_runtime.approval_requested",
    ),
  );
  const approval = events.find(
    (event) => event.data.event_type === "agent_runtime.approval_requested",
  ).data.payload;
  assert.equal(
    (await ARTIFACTS.list()).objects.length,
    blobsBefore,
    "the empty unfinished-turn marker must stay in Durable Object storage",
  );
  assert.equal(await (await sandboxFor(agent, thread)).count(), 0);
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

test("shared runtime serializes concurrent submissions and cancellation prevents pending tools", async () => {
  const { agent, thread, path } = await create(true);
  const first = await sendTurn(path);
  const second = sendTurn(path, "Print Linux again.");
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
  await waitTurn(path, submitted.turn.id);
  assert.deepEqual(await api(`${path}/turn/${submitted.turn.id}`), {
    active: false,
  });
  assert.equal(await (await sandboxFor(agent, thread)).count(), 0);
  const events = (await api(`${path}/event`)).events;
  assert.equal(
    events.filter((event) => event.data.type === "turn_started").length,
    2,
  );
});

async function watch(path, after) {
  const abort = new AbortController();
  const response = await mf.dispatchFetch(
    `https://exo.test/exo/${path}/event/watch${after ? `?after=${after}` : ""}`,
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
  return { received, collect, cancel: () => abort.abort() };
}

test("SSE replays a cursor and streams new events without duplicates", async () => {
  const { path } = await create();
  const first = (await api(`${path}/event`)).events[0];
  const { received, collect, cancel } = await watch(path, first.id);
  try {
    await sendTurn(path);
    await collect;
    assert(received.some((event) => event.data.type === "tool_result"));
    assert(!received.some((event) => event.id === first.id));
    assert.equal(
      new Set(received.map((event) => event.id)).size,
      received.length,
    );
  } finally {
    cancel();
  }
});

test("Codex reuses its RPC process across turns and resumes after backend shutdown", async () => {
  const { agent, thread, path } = await create(false, "codex");
  const streaming = await watch(path);
  const first = await sendTurn(path, "run tests");
  assert.equal(first.harness, "codex-harness");
  const events = await waitTurn(path);
  await assertTurnInactive(thread.id);
  assert.match(
    (await sandboxRecord(agent, thread)).image,
    /^codex-[0-9a-f]{8}$/,
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
  assert(
    JSON.stringify(events).includes("Done. 🧪"),
    "process chunks must preserve split UTF-8",
  );
  await streaming.collect;
  streaming.cancel();
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
  assert(!events.some((event) => event.data.event_type === "codex_text_delta"));
  const sandbox = await sandboxFor(agent, thread);
  assert.equal(await sandbox.activityCount(), 0);
  assert.equal(await sandbox.lastMethod(), "thread/start");

  const second = await sendTurn(path, "continue");
  const finished = await waitTurn(path, second.turn.id);
  assert(
    !finished.some((event) => event.data.type === "error"),
    JSON.stringify(finished),
  );
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
    (event) => event.data.type === "sandbox_process_started",
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
  assert(
    !resumed.some((event) => event.data.type === "error"),
    JSON.stringify(resumed),
  );
  assert.equal(
    resumed.filter((event) => event.data.type === "sandbox_process_started")
      .length,
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
});

test("Codex cancellation uses the shared turn lifecycle and stops its managed process", async () => {
  const { agent, thread, path } = await create(false, "codex");
  const submitted = await sendTurn(path, "hold-turn");
  const events = await waitEvents(path, (events) =>
    events.some((event) => event.data.event_type === "codex_turn_started"),
  );
  assert(!events.some((event) => event.data.type === "turn_ended"));
  const sandbox = await sandboxFor(agent, thread);
  assert.equal(await sandbox.activityCount(), 1);
  const { PROVIDERS } = await mf.getBindings("exo");
  const executed = await PROVIDERS.getByName(
    "test-account",
  ).harnessRequestForTest(thread.id, {
    type: "execute_tool",
    request: { function_name: "shell", arguments: { command: "uname -s" } },
  });
  assert.equal(executed.type, "tool_result");
  assert.equal(executed.result.stdout, "Linux test\n");
  const process = events.find(
    (event) => event.data.type === "sandbox_process_started",
  ).data;
  assert.equal(
    (await api(`${path}/turn/${submitted.turn.id}/cancel`, "POST", {}))
      .canceled_active_turn,
    true,
  );
  await waitTurn(path);
  await assertTurnInactive(thread.id);
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
});

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
  for (
    let i = 0;
    i < 100 && (await SANDBOXES.getByName(sandbox_id).processCount());
    i++
  )
    await new Promise((resolve) => setTimeout(resolve, 10));
  assert.equal(await SANDBOXES.getByName(sandbox_id).processCount(), 0);
});

test("existing Rust trait contracts run against the Worker store", async () => {
  const endpoint = new URL("/exo", await mf.ready).href;
  const child = spawn(
    "cargo",
    [
      "test",
      "-p",
      "exoharness",
      "--features",
      "basic-backend",
      "hosted_http_exoharness_core_contract",
      "--",
      "--ignored",
      "--exact",
      "http_tests::hosted_http_exoharness_core_contract",
    ],
    {
      cwd: fileURLToPath(new URL("../../../", import.meta.url)),
      env: {
        ...process.env,
        EXO_CONTRACT_TEST_URL: endpoint,
        EXO_CONTRACT_TEST_BEARER: token,
      },
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
  let output = "";
  child.stdout.on("data", (chunk) => (output += chunk));
  child.stderr.on("data", (chunk) => (output += chunk));
  const code = await new Promise((resolve, reject) => {
    child.on("error", reject);
    child.on("close", resolve);
  });
  assert.equal(code, 0, output);
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
    assert.match((await proxy(identity, url, headers)).error, message, url);
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
  await sandbox.stop();
});
