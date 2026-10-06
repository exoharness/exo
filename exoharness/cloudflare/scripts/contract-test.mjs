import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

export async function runContracts(endpoint, token) {
  const child = spawn(
    "cargo",
    [
      "test",
      "--locked",
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
  return output;
}
