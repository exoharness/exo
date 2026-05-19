import { execFileSync } from "node:child_process";
import path from "node:path";
import process from "node:process";

const formatExtensions = new Set([
  ".cjs",
  ".css",
  ".js",
  ".json",
  ".jsx",
  ".md",
  ".mjs",
  ".ts",
  ".tsx",
  ".yaml",
  ".yml",
]);

const lintExtensions = new Set([".cjs", ".js", ".jsx", ".mjs", ".ts", ".tsx"]);

function run(command, args) {
  execFileSync(command, args, {
    stdio: "inherit",
  });
}

function getStagedFiles() {
  const output = execFileSync(
    "git",
    ["diff", "--cached", "--name-only", "--diff-filter=ACMR"],
    {
      encoding: "utf8",
    },
  ).trim();

  return output.length > 0 ? output.split("\n").filter(Boolean) : [];
}

function filterByExtension(files, extensions) {
  return files.filter((file) => extensions.has(path.extname(file)));
}

const stagedFiles = getStagedFiles();

if (stagedFiles.length === 0) {
  process.exit(0);
}

const formatFiles = filterByExtension(stagedFiles, formatExtensions);
if (formatFiles.length > 0) {
  run("pnpm", ["exec", "oxfmt", ...formatFiles]);
  run("git", ["add", "--", ...formatFiles]);
}

const lintFiles = filterByExtension(stagedFiles, lintExtensions);
if (lintFiles.length > 0) {
  run("pnpm", ["exec", "oxlint", "--fix", ...lintFiles]);
  run("git", ["add", "--", ...lintFiles]);
}

run("pnpm", ["check"]);
