import { readdirSync, readFileSync, statSync } from "node:fs";
import path from "node:path";

import type {
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  ToolInstance,
} from "@exo/harness";

const DEFAULT_REPO_PATH = "/workspace/exo";
const EXCLUDED_DIRS = new Set([
  ".git",
  ".exo",
  "target",
  "node_modules",
  ".turbo",
  "dist",
  "build",
  ".next",
]);
const SOURCE_EXTENSIONS = new Set([
  ".rs",
  ".ts",
  ".tsx",
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".sh",
]);
const TEXT_EXTENSIONS = new Set([
  ".rs",
  ".ts",
  ".tsx",
  ".js",
  ".jsx",
  ".mjs",
  ".cjs",
  ".md",
  ".toml",
  ".json",
  ".yml",
  ".yaml",
  ".sh",
  ".html",
  ".css",
]);
const IGNORED_TEXT_FILES = new Set([
  "Cargo.lock",
  "pnpm-lock.yaml",
  "tsconfig.tsbuildinfo",
]);
const ISSUE_MARKERS = ["TO" + "DO", "FIX" + "ME"];

function issueMarkerLabel(): string {
  return ISSUE_MARKERS.join("/");
}

function issueMarkerPattern(): RegExp {
  return new RegExp("\\b(?:" + ISSUE_MARKERS.join("|") + ")\\b", "i");
}

export function registerRepoHealthTool(registry: HarnessToolRegistry): void {
  registry.register(createRepoHealthTool());
}

function createRepoHealthTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "repo_health_report_data",
      description:
        "Scan the exo repository once and return compact, citation-ready data for the recurring repo health report: largest source files, issue-marker census, Rust and Node dependency inventory, Rust/TypeScript test inventory, and architecture summary. Prefer this over ad-hoc shell scans for repo health reports.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          repoPath: {
            anyOf: [{ type: "string" }, { type: "null" }],
            description:
              "Path to the exo repository. Defaults to /workspace/exo.",
          },
        },
        required: ["repoPath"],
      },
    },
    handler: {
      async execute(args) {
        const repoPath =
          typeof args.repoPath === "string" && args.repoPath.length > 0
            ? args.repoPath
            : DEFAULT_REPO_PATH;
        const data = buildRepoHealthData(repoPath);
        return {
          _exoDirectFinal: markdownReport(data),
          generatedBy: "repo_health_report_data",
          repoPath,
        } as JsonObject;
      },
    },
  };
}

export function buildRepoHealthData(repoPath: string): JsonObject {
  const files = walkFiles(repoPath);
  const largest = largestSourceFiles(repoPath, files);
  const todos = todoCensus(repoPath, files);
  const dependencies = dependencyInventory(repoPath);
  const tests = testInventory(repoPath, files);
  return {
    repoPath,
    scanExcludes: [...EXCLUDED_DIRS].sort(),
    largestSourceFiles: largest,
    todoFixmeCensus: {
      count: todos.length,
      items: todos,
    },
    dependencyInventory: dependencies,
    testInventory: tests,
    architectureSummary: architectureSummary(),
  };
}

function walkFiles(root: string): string[] {
  const out: string[] = [];
  function visit(dir: string): void {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (entry.name === "." || entry.name === "..") {
        continue;
      }
      if (entry.isDirectory() && EXCLUDED_DIRS.has(entry.name)) {
        continue;
      }
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        visit(fullPath);
      } else if (entry.isFile()) {
        out.push(fullPath);
      }
    }
  }
  visit(root);
  return out;
}

function rel(root: string, file: string): string {
  return path.relative(root, file).replaceAll(path.sep, "/");
}

function readText(file: string): string {
  return readFileSync(file, "utf8");
}

function lineCount(text: string): number {
  if (text.length === 0) {
    return 0;
  }
  const newlineCount = text.match(/\n/g)?.length ?? 0;
  return text.endsWith("\n") ? newlineCount : newlineCount + 1;
}

function largestSourceFiles(root: string, files: string[]): JsonValue[] {
  return files
    .filter((file) => SOURCE_EXTENSIONS.has(path.extname(file)))
    .map((file) => ({
      path: rel(root, file),
      lines: lineCount(readText(file)),
    }))
    .sort((a, b) => b.lines - a.lines || a.path.localeCompare(b.path))
    .slice(0, 10) as JsonValue[];
}

function todoCensus(root: string, files: string[]): JsonValue[] {
  const items: JsonObject[] = [];
  for (const file of files) {
    if (
      IGNORED_TEXT_FILES.has(path.basename(file)) ||
      !TEXT_EXTENSIONS.has(path.extname(file))
    ) {
      continue;
    }
    const lines = readText(file).split(/\n/);
    lines.forEach((line, index) => {
      if (issueMarkerPattern().test(line)) {
        items.push({
          path: rel(root, file),
          line: index + 1,
          text: line.trim(),
        });
      }
    });
  }
  return items;
}

function dependencyInventory(root: string): JsonObject {
  const workspaceMembers = workspaceMembersFromRoot(root);
  const crateManifests = workspaceMembers
    .map((member) => path.join(root, member, "Cargo.toml"))
    .filter((manifest) => existsFile(manifest));
  const rustCrates = crateManifests.map((manifest) =>
    rustCrateDeps(root, manifest),
  );
  return {
    rustWorkspaceMembers: workspaceMembers,
    rustCrates,
    node: nodeDependencies(root),
  };
}

function existsFile(file: string): boolean {
  try {
    return statSync(file).isFile();
  } catch {
    return false;
  }
}

function workspaceMembersFromRoot(root: string): string[] {
  const toml = readText(path.join(root, "Cargo.toml"));
  const membersBlock = toml.match(/members\s*=\s*\[([\s\S]*?)\]/m)?.[1] ?? "";
  return [...membersBlock.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
}

function rustCrateDeps(root: string, manifest: string): JsonObject {
  const toml = readText(manifest);
  const crate =
    toml.match(/^name\s*=\s*"([^"]+)"/m)?.[1] ??
    path.basename(path.dirname(manifest));
  const normal = dependenciesInSections(
    toml,
    /^(dependencies|target\..*\.dependencies)$/,
  );
  const dev = dependenciesInSections(toml, /^dev-dependencies$/);
  const build = dependenciesInSections(toml, /^build-dependencies$/);
  return {
    crate,
    manifest: rel(root, manifest),
    dependencyCount: normal.length,
    devDependencyCount: dev.length,
    buildDependencyCount: build.length,
    totalDependencyCount: normal.length + dev.length + build.length,
    dependencies: normal,
    devDependencies: dev,
    buildDependencies: build,
  };
}

function dependenciesInSections(
  toml: string,
  sectionPattern: RegExp,
): string[] {
  const deps: string[] = [];
  let inSection = false;
  for (const rawLine of toml.split(/\n/)) {
    const section = rawLine.match(/^\s*\[([^\]]+)\]\s*$/)?.[1];
    if (section !== undefined) {
      inSection = sectionPattern.test(section);
      continue;
    }
    if (!inSection) {
      continue;
    }
    const line = rawLine.replace(/#.*/, "").trim();
    if (line.length === 0) {
      continue;
    }
    const dep = line.match(/^([A-Za-z0-9_-]+)(?:\.workspace)?\s*=/)?.[1];
    if (dep) {
      deps.push(dep);
    }
  }
  return deps;
}

function nodeDependencies(root: string): JsonObject {
  const pkg = JSON.parse(readText(path.join(root, "package.json"))) as Record<
    string,
    Record<string, string> | undefined
  >;
  const section = (name: string): JsonObject => {
    const entries = Object.entries(pkg[name] ?? {}).sort(([a], [b]) =>
      a.localeCompare(b),
    );
    return {
      count: entries.length,
      dependencies: entries.map(([dependency, version]) => ({
        dependency,
        version,
      })),
    };
  };
  return {
    dependencies: section("dependencies"),
    devDependencies: section("devDependencies"),
    peerDependencies: section("peerDependencies"),
    optionalDependencies: section("optionalDependencies"),
    packageManager:
      typeof pkg.packageManager === "string" ? pkg.packageManager : null,
  };
}

function testInventory(root: string, files: string[]): JsonObject {
  const rustItems = files
    .filter((file) =>
      /\/(?:crates|tests)\/.*\.rs$/.test(file.replaceAll(path.sep, "/")),
    )
    .map((file) => {
      const text = readText(file);
      const testCount =
        text.match(/#\s*\[\s*(?:[A-Za-z0-9_]+::)?test\b/g)?.length ?? 0;
      const hasTestsModule = /mod\s+tests\b/.test(text);
      return { path: rel(root, file), testCount, hasTestsModule };
    })
    .filter((item) => item.testCount > 0 || item.hasTestsModule)
    .sort((a, b) => a.path.localeCompare(b.path));

  const tsItems = files
    .filter((file) => file.endsWith(".test.ts"))
    .map((file) => {
      const text = readText(file);
      return {
        path: rel(root, file),
        testCount: text.match(/\b(?:it|test)\s*\(/g)?.length ?? 0,
        describeCount: text.match(/\bdescribe\s*\(/g)?.length ?? 0,
      };
    })
    .sort((a, b) => a.path.localeCompare(b.path));

  return {
    rust: {
      fileCount: rustItems.length,
      testFunctionCount: rustItems.reduce(
        (sum, item) => sum + item.testCount,
        0,
      ),
      files: rustItems,
    },
    typescript: {
      fileCount: tsItems.length,
      testCaseCount: tsItems.reduce((sum, item) => sum + item.testCount, 0),
      files: tsItems,
    },
  };
}

export function markdownReport(data: JsonObject): string {
  const largest = data.largestSourceFiles as Array<{
    path: string;
    lines: number;
  }>;
  const issueCensus = data.todoFixmeCensus as {
    count: number;
    items: Array<{ path: string; line: number; text: string }>;
  };
  const deps = data.dependencyInventory as {
    rustWorkspaceMembers: string[];
    rustCrates: Array<{
      crate: string;
      manifest: string;
      dependencyCount: number;
      devDependencyCount: number;
      buildDependencyCount: number;
      totalDependencyCount: number;
    }>;
    node: {
      dependencies: {
        count: number;
        dependencies: Array<{ dependency: string; version: string }>;
      };
      devDependencies: {
        count: number;
        dependencies: Array<{ dependency: string; version: string }>;
      };
      peerDependencies: { count: number };
      optionalDependencies: { count: number };
    };
  };
  const tests = data.testInventory as {
    rust: {
      fileCount: number;
      testFunctionCount: number;
      files: Array<{ path: string; testCount: number }>;
    };
    typescript: {
      fileCount: number;
      testCaseCount: number;
      files: Array<{ path: string; testCount: number; describeCount: number }>;
    };
  };
  const lines: string[] = [];
  lines.push(`Repo health report for \`${data.repoPath as string}\``);
  lines.push(
    `Scope: static scan excluding \`${(data.scanExcludes as string[]).join("`, `")}\`.`,
  );
  lines.push("");
  lines.push("## 1) Ten largest source files by line count");
  lines.push("| Rank | Lines | Path |");
  lines.push("|---:|---:|---|");
  largest.forEach((item, index) =>
    lines.push(
      `| ${index + 1} | ${item.lines.toLocaleString("en-US")} | \`${item.path}\` |`,
    ),
  );
  lines.push("");
  lines.push(`## 2) ${issueMarkerLabel()} census`);
  lines.push(`Found ${issueCensus.count} ${issueMarkerLabel()} references:`);
  lines.push("");
  for (const item of issueCensus.items) {
    lines.push(`- \`${item.path}:${item.line}\`: \`${item.text}\``);
  }
  lines.push("");
  lines.push("## 3) Dependency inventory");
  lines.push("### Rust workspace crates");
  lines.push(
    "Workspace members: " +
      deps.rustWorkspaceMembers.map((m) => `\`${m}\``).join(", ") +
      ".",
  );
  lines.push("");
  lines.push(
    "| Crate | Manifest | Normal deps | Dev deps | Build deps | Total |",
  );
  lines.push("|---|---|---:|---:|---:|---:|");
  for (const crate of deps.rustCrates) {
    lines.push(
      `| \`${crate.crate}\` | \`${crate.manifest}\` | ${crate.dependencyCount} | ${crate.devDependencyCount} | ${crate.buildDependencyCount} | ${crate.totalDependencyCount} |`,
    );
  }
  lines.push("");
  lines.push("### Node dependencies from `package.json`");
  lines.push(`Runtime dependencies: ${deps.node.dependencies.count}`);
  deps.node.dependencies.dependencies.forEach((dep) =>
    lines.push(`- \`${dep.dependency}\`: \`${dep.version}\``),
  );
  lines.push("");
  lines.push(`Dev dependencies: ${deps.node.devDependencies.count}`);
  deps.node.devDependencies.dependencies.forEach((dep) =>
    lines.push(`- \`${dep.dependency}\`: \`${dep.version}\``),
  );
  lines.push("");
  lines.push(`Peer dependencies: ${deps.node.peerDependencies.count}`);
  lines.push(`Optional dependencies: ${deps.node.optionalDependencies.count}`);
  lines.push("");
  lines.push("## 4) Test inventory");
  lines.push(`### Rust files containing tests`);
  lines.push(
    `Total: ${tests.rust.fileCount} Rust files containing tests, with ${tests.rust.testFunctionCount} test functions.`,
  );
  lines.push("");
  lines.push("| Test count | Path |");
  lines.push("|---:|---|");
  tests.rust.files.forEach((file) =>
    lines.push(`| ${file.testCount} | \`${file.path}\` |`),
  );
  lines.push("");
  lines.push("### TypeScript `*.test.ts` files");
  lines.push(
    `Total: ${tests.typescript.fileCount} TS test files, with ${tests.typescript.testCaseCount} test cases.`,
  );
  lines.push("");
  lines.push("| Test cases | `describe(...)` blocks | Path |");
  lines.push("|---:|---:|---|");
  tests.typescript.files.forEach((file) =>
    lines.push(
      `| ${file.testCount} | ${file.describeCount} | \`${file.path}\` |`,
    ),
  );
  lines.push("");
  lines.push("## 5) Architecture summary");
  lines.push(data.architectureSummary as string);
  return lines.join("\n");
}

function architectureSummary(): string {
  return [
    "exo is organized around a durable substrate plus a swappable execution policy layer. crates/exoharness is the substrate: it owns persistent agent/conversation state, event history, artifacts, secrets, snapshots, and sandbox/provider abstractions. Its large basic, sandbox, and provider files implement the backend and isolation machinery.",
    "crates/executor sits above that substrate and implements runtime behavior: prompt/tool execution, model routing hooks, TypeScript harness loading, local sandbox control, scheduler runtime/store/types, conversation events and wakeups, and adapter runtime/store/tools under crates/executor/src/adapter.",
    "crates/cli builds the exo operator binary. It exposes the local control surface for REPL/chat, secrets, models, sandbox mounting, snapshots, adapters, scheduler operations, and backend integration flows, backed by executor and exoharness.",
    "The TypeScript tree provides the user-facing harness API and model-runtime utilities. typescript/harness defines tool/module interfaces, built-in and adapter tools, and the TS runner; typescript/model-runtime handles response plumbing and cost accounting. examples/ contains concrete harnesses, including examples/typescript/codex-harness.ts and examples/exoclaw for the long-running Scarab/Exoclaw control-agent setup.",
  ].join(" ");
}

export function buildRepoHealthMarkdownReport(
  repoPath = DEFAULT_REPO_PATH,
): string {
  return markdownReport(buildRepoHealthData(repoPath));
}
