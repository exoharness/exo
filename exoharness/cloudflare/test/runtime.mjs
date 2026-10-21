import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import ts from "typescript";

// Exercise the production stream adapter without instantiating its wasm host.
const source = (
  await readFile(new URL("../src/runtime.ts", import.meta.url), "utf8")
)
  .replace(
    /import \{ initSync, WorkerRuntime \} from .*;\nimport runtimeWasm from .*;\n/,
    "const WorkerRuntime = class {};\n",
  )
  .replace("initSync({ module: runtimeWasm });", "");
const { Runtime } = await import(
  `data:text/javascript;base64,${Buffer.from(
    ts.transpileModule(source, {
      compilerOptions: {
        module: ts.ModuleKind.ESNext,
        target: ts.ScriptTarget.ES2022,
      },
    }).outputText,
  ).toString("base64")}`
);

test("external watch abort rejects pending readers and closes late opening handles", async () => {
  for (const pendingOpening of [false, true]) {
    const runtime = Object.create(Runtime.prototype);
    const cleanup = [];
    let closed;
    runtime.waitUntil = (promise) => cleanup.push(promise);
    runtime.call = async (operation, signal) => {
      if (operation.type === "watch_close") {
        closed = operation.id;
        return;
      }
      signal.throwIfAborted();
      return new Promise((_, reject) =>
        signal.addEventListener("abort", () => reject(signal.reason), {
          once: true,
        }),
      );
    };
    let opened;
    const opening = pendingOpening
      ? new Promise((resolve) => {
          opened = resolve;
        })
      : Promise.resolve(42);
    const abort = new AbortController();
    const reader = runtime.stream(opening, abort.signal).getReader();
    const read = reader.read();
    const reason = new Error("client disconnected");
    abort.abort(reason);
    await assert.rejects(read, (error) => error === reason);
    opened?.(42);
    await Promise.all(cleanup);
    assert.equal(closed, 42);
  }
});
