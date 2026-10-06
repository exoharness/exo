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
const { ExoSandbox } = await import(
  `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
);
const identity = { agentId: "agent", threadId: "thread", sandboxId: "sandbox" };
const codexImage = `registry.cloudflare.com/test/exo-codex-devbox@sha256:${"a".repeat(64)}`;
const codexName = "codex-aaaaaaaa";

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
    images: { [codexName]: codexImage },
    interceptAllOutboundHttp: async () => {},
    interceptOutboundHttps: async () => {},
    start: (options) => {
      calls.push(["start", options]);
      container.running = true;
    },
    exec: async (command) => {
      calls.push(["exec", command]);
      return process;
    },
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
  await f.sandbox.acquire(identity, "", "/workspace", {}, null, 300_000);
  assert.equal(
    f.calls.find((call) => call[0] === "start")[1].image,
    "cloudflare/debian-trixie",
  );
  assert.deepEqual(
    f.calls.filter((call) => call[0] === "exec"),
    [["exec", ["mkdir", "-p", "/workspace"]]],
  );
  await assert.rejects(
    f.sandbox.acquire(identity, "unknown", "/workspace", {}, null, 300_000),
    /image is not configured/,
  );
  await f.sandbox.beginActivity(identity, "first");
  await f.sandbox.beginActivity(identity, "overlapping");
  await f.sandbox.endActivity("first");
  await f.storage.setAlarm(Date.now() - 1);
  await f.sandbox.alarm();
  assert(!f.calls.includes("snapshot"));
  await f.sandbox.endActivity("overlapping");
  await f.sandbox.beginActivity(identity, "second");
  await f.sandbox.endActivity("second");
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
  await f.sandbox.beginActivity(identity, "third");
  assert.deepEqual(
    f.calls
      .filter((call) => Array.isArray(call) && call[0] === "start")
      .at(-1)[1].containerSnapshot,
    { id: "checkpoint" },
  );
});

test("production process invocation stays alive; checkpoint finishes before new work", async () => {
  const f = fixture();
  await f.sandbox.acquire(identity, codexName, "/workspace", {}, null, 300_000);
  assert.equal(
    f.calls.find((call) => call[0] === "start")[1].image,
    codexImage,
  );
  assert.equal(await f.storage.get("image"), codexImage);
  await f.sandbox.beginActivity(identity, "first");
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
  await f.sandbox.endActivity("first");
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
  const next = f.sandbox.beginActivity(identity, "second").then(() => {
    began = true;
  });
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(began, false);
  release();
  await stop;
  await next;
  assert.equal(began, true);
});

test("overlapping acquire, activity and snapshot share one container start", async () => {
  const f = fixture();
  await f.sandbox.acquire(identity, "", "/workspace", {}, null, 300_000);
  f.container.running = false;
  await Promise.all([
    f.sandbox.acquire(identity, "", "/workspace", {}, null, 300_000),
    f.sandbox.beginActivity(identity, "turn"),
    f.sandbox.snapshot(identity),
  ]);
  assert.equal(f.calls.filter((call) => call[0] === "start").length, 2);
});

test("a command timeout kills the process and preserves the warm container", async () => {
  const f = fixture();
  await f.sandbox.acquire(identity, "", "/workspace", {}, null, 300_000);
  let finish;
  const output = new Promise((resolve) => {
    finish = resolve;
  });
  f.container.exec = async () => ({
    exitCode: output.then((result) => result.exitCode),
    output: () => output,
    kill: (signal) => {
      f.calls.push(`kill:${signal}`);
      finish({
        exitCode: 137,
        stdout: new Uint8Array(),
        stderr: new Uint8Array(),
      });
    },
  });
  const result = await f.sandbox.exec(identity, {
    command: ["sleep", "10"],
    timeoutMs: 1,
  });
  assert.equal(result.exitCode, 137);
  assert(f.calls.includes("kill:9"));
  assert(!f.calls.includes("destroy"));
  assert(f.container.running);
});
