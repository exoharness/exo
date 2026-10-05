import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { base } from "./live-api.mjs";

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
