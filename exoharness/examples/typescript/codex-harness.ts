import { readFileSync } from "node:fs";
import { createCodexHarness } from "../../typescript/codex/harness";

export default createCodexHarness(
  readFileSync(
    new URL("../../containers/codex-sandbox/version", import.meta.url),
    "utf8",
  ).trim(),
);
