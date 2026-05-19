import { existsSync } from "node:fs";
import { execFileSync } from "node:child_process";
import process from "node:process";

if (!existsSync(".git")) {
  process.exit(0);
}

try {
  execFileSync("git", ["rev-parse", "--is-inside-work-tree"], {
    stdio: "ignore",
  });
} catch {
  process.exit(0);
}

execFileSync("git", ["config", "core.hooksPath", ".githooks"], {
  stdio: "inherit",
});
