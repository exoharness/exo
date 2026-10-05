import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";

const base = process.env.EXO_WORKER_URL;
assert(base, "EXO_WORKER_URL is required (the Worker origin, without /exo)");
assert(process.env.EXO_TOKEN, "EXO_TOKEN is required");
const result = spawnSync(
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
    stdio: "inherit",
    env: {
      ...process.env,
      EXO_CONTRACT_TEST_URL: `${base}/exo`,
      EXO_CONTRACT_TEST_BEARER: process.env.EXO_TOKEN,
    },
  },
);
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
