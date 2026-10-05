import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import ts from "typescript";
import { test } from "node:test";

// Exercise the production adapter with the platform container API replaced.
// The fixture does not implement turn, idle or checkpoint behavior itself.
let source = ts.transpileModule(
  await readFile(new URL("../src/sandbox.ts", import.meta.url), "utf8"),
  {
    compilerOptions: {
      module: ts.ModuleKind.ESNext,
      target: ts.ScriptTarget.ES2022,
    },
  },
).outputText;
source = source.replace(
  'import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";',
  `
class DurableObject { constructor(ctx, env) { this.ctx = ctx; this.env = env; } }
class RpcTarget {}
class RpcStub { constructor(target) { target[Symbol.dispose] = () => {}; return target; } }
`,
);
source = source.replace(
  'import codexPackage from "./codex-package.json";',
  `const codexPackage = ${await readFile(new URL("../src/codex-package.json", import.meta.url), "utf8")};`,
);
const { ExoSandbox } = await import(
  `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
);
const identity = { agentId: "agent", threadId: "thread", sandboxId: "sandbox" };

function fixture() {
  const data = new Map();
  const calls = [];
  let alarm = null;
  let checkpoint;
  const storage = {
    get: async (key) => data.get(key),
    put: async (key, value) => data.set(key, value),
    deleteAll: async () => data.clear(),
    getAlarm: async () => alarm,
    setAlarm: async (value) => {
      alarm = value;
    },
    deleteAlarm: async () => {
      alarm = null;
    },
  };
  let finish;
  const exitCode = new Promise((resolve) => {
    finish = resolve;
  });
  const process = {
    pid: 1,
    stdin: new WritableStream(),
    stdout: new ReadableStream(),
    stderr: new ReadableStream(),
    exitCode,
    kill: (signal) => {
      calls.push(`kill:${signal}`);
      finish(0);
    },
    output: async () => ({
      exitCode: 0,
      stdout: new Uint8Array(),
      stderr: new Uint8Array(),
    }),
  };
  const container = {
    running: false,
    interceptAllOutboundHttp: async () => {},
    interceptOutboundHttps: async () => {},
    start: (options) => {
      calls.push(["start", options]);
      container.running = true;
    },
    exec: async () => process,
    setInactivityTimeout: async (value) => calls.push(["timeout", value]),
    snapshotContainer: async () => {
      calls.push("snapshot");
      await checkpoint;
      return { id: "checkpoint", size: 1 };
    },
    destroy: async () => {
      calls.push("destroy");
      container.running = false;
    },
  };
  const ctx = {
    storage,
    container,
    exports: { ExoEgress: () => ({}) },
    waitUntil: () => {},
  };
  const env = {
    ACCOUNT_ID: "account",
    PROVIDERS: {
      getByName: () => ({
        sandboxPolicy: async () => ({ MODEL_TOKEN: "placeholder" }),
      }),
    },
  };
  return {
    sandbox: new ExoSandbox(ctx, env),
    container,
    calls,
    storage,
    finish,
    delayCheckpoint: (promise) => {
      checkpoint = promise;
    },
  };
}

test("production sandbox stays warm between turns and checkpoints on idle", async () => {
  const f = fixture();
  await f.sandbox.beginTurn(identity, "first");
  await f.sandbox.endTurn("first");
  await f.sandbox.beginTurn(identity, "second");
  await f.sandbox.endTurn("second");
  assert.equal(
    f.calls.filter((call) => Array.isArray(call) && call[0] === "start").length,
    1,
  );
  assert(!f.calls.includes("snapshot"));
  await f.storage.setAlarm(Date.now() - 1);
  await f.sandbox.alarm();
  assert.deepEqual(
    f.calls.filter((call) => typeof call === "string"),
    ["snapshot", "destroy"],
  );
  await f.sandbox.beginTurn(identity, "third");
  assert.deepEqual(
    f.calls
      .filter((call) => Array.isArray(call) && call[0] === "start")
      .at(-1)[1].containerSnapshot,
    { id: "checkpoint" },
  );
});

test("production process invocation stays alive; checkpoint finishes before new work", async () => {
  const f = fixture();
  await f.sandbox.beginTurn(identity, "first");
  let ready;
  const started = new Promise((resolve) => {
    ready = resolve;
  });
  let finished = false;
  const invocation = f.sandbox
    .runProcess(
      identity,
      { command: ["codex", "app-server"], env: {} },
      async (process) => ready(process),
    )
    .then(() => {
      finished = true;
    });
  await started;
  await f.sandbox.endTurn("first");
  assert.equal(finished, false);
  await f.storage.setAlarm(Date.now() - 1);
  let release;
  f.delayCheckpoint(
    new Promise((resolve) => {
      release = resolve;
    }),
  );
  const stop = f.sandbox.alarm();
  await invocation;
  assert(f.calls.includes("kill:15"));
  let began = false;
  const next = f.sandbox.beginTurn(identity, "second").then(() => {
    began = true;
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(began, false);
  release();
  await stop;
  await next;
  assert.equal(began, true);
});
