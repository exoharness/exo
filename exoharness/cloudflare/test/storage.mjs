import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";
import ts from "typescript";
const source = ts.transpileModule(
  await readFile(new URL("../src/storage.ts", import.meta.url), "utf8"),
  {
    compilerOptions: {
      module: ts.ModuleKind.ESNext,
      target: ts.ScriptTarget.ES2022,
    },
  },
).outputText;
const { Storage } = await import(
  `data:text/javascript;base64,${Buffer.from(source).toString("base64")}`
);

test("blob mutations serialize replacement and preserve committed writes when cleanup fails", async (t) => {
  const logs = t.mock.method(console, "error", () => {});
  const kvData = new Map(),
    blobData = new Map();
  let failure = false;
  let cleanupFailure = false;
  const kv = {
    get: async (key) => kvData.get(key),
    put: async (key, value) => kvData.set(key, value),
    delete: async (key) => kvData.delete(key),
    transaction: async (callback) => {
      if (failure) throw new Error("storage unavailable");
      return callback(kv);
    },
  };
  const blobs = {
    put: async (key, bytes) => {
      await Promise.resolve();
      blobData.set(key, bytes);
    },
    get: async (key) =>
      blobData.has(key)
        ? { arrayBuffer: async () => blobData.get(key).buffer }
        : null,
    delete: async (key) => {
      if (cleanupFailure) throw new Error("R2 deletion unavailable");
      blobData.delete(key);
    },
  };
  const store = new Storage(kv, blobs, "account");
  const put = (n) =>
    store.handle({
      type: "put",
      key: "artifact",
      bytes: new Uint8Array([n]),
      blob: true,
      alarm: null,
    });
  await Promise.all([put(1), put(2), put(3)]);
  assert.deepEqual(
    await store.handle({ type: "get", key: "artifact" }),
    new Uint8Array([3]),
  );
  assert.equal(blobData.size, 1);
  failure = true;
  await assert.rejects(put(4), /storage unavailable/);
  assert.equal(blobData.size, 1, "failed commits remove their uploaded object");
  failure = false;
  await Promise.all([
    put(5),
    store.handle({ type: "delete", key: "artifact" }),
  ]);
  assert.equal(await store.handle({ type: "get", key: "artifact" }), null);
  assert.equal(blobData.size, 0);
  await put(6);
  cleanupFailure = true;
  await put(7);
  assert.deepEqual(
    await store.handle({ type: "get", key: "artifact" }),
    new Uint8Array([7]),
    "cleanup failure must not reject an already committed write",
  );
  await store.handle({ type: "delete", key: "artifact" });
  assert.equal(await store.handle({ type: "get", key: "artifact" }), null);
  assert.equal(logs.mock.callCount(), 2);
});
