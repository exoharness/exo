import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { resolve } from "node:path";

const root = fileURLToPath(new URL("../../../", import.meta.url));
const output = resolve(root, "exoharness/cloudflare/src/wasm");
const bindgen =
  process.env.WASM_BINDGEN_BIN ??
  resolve(root, "exoharness/cloudflare/.local/tools/bin/wasm-bindgen");
function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}
mkdirSync(output, { recursive: true });
run("cargo", [
  "build",
  "-p",
  "exo-worker-runtime",
  "--release",
  "--target",
  "wasm32-unknown-unknown",
  "--target-dir",
  resolve(root, "target"),
]);
run(bindgen, [
  resolve(
    root,
    "target/wasm32-unknown-unknown/release/exo_worker_runtime.wasm",
  ),
  "--target",
  "web",
  "--out-dir",
  output,
  "--out-name",
  "exo_worker_runtime",
]);
