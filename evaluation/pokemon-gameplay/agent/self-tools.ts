// The self-improvement surface: the agent edits its own playbook (prompt
// injection), saves durable memories, maintains a todo list, and authors new
// tools that are hot-loaded into its own registry. Mirrors exo's
// architecture, scoped to this evaluation.

import fs from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

import type { AgentTool, AgentToolContext, ToolResult } from "./tool-types";

const NAME_RE = /^[a-z][a-z0-9_-]{0,63}$/;
const PLAYBOOK_MAX_CHARS = 16_000;
const MEMORY_MAX_CHARS = 16_000;
const TOOL_SOURCE_MAX_CHARS = 32_000;

export class SelfStore {
  readonly playbookPath: string;
  readonly memoryDir: string;
  readonly todosPath: string;
  readonly toolsDir: string;

  constructor(runtimeDir: string) {
    this.playbookPath = path.join(runtimeDir, "playbook.md");
    this.memoryDir = path.join(runtimeDir, "memory");
    this.todosPath = path.join(runtimeDir, "todos.json");
    this.toolsDir = path.join(runtimeDir, "tools");
  }

  async init(seedPlaybook: string): Promise<void> {
    await fs.mkdir(this.memoryDir, { recursive: true });
    await fs.mkdir(this.toolsDir, { recursive: true });
    try {
      await fs.access(this.playbookPath);
    } catch {
      await fs.writeFile(this.playbookPath, seedPlaybook, "utf8");
    }
  }

  async playbook(): Promise<string> {
    return (await fs.readFile(this.playbookPath, "utf8")).trim();
  }

  async todos(): Promise<{ text: string; status: string }[]> {
    try {
      const parsed = JSON.parse(await fs.readFile(this.todosPath, "utf8"));
      return Array.isArray(parsed) ? parsed : [];
    } catch {
      return [];
    }
  }

  async memoryIndex(): Promise<{ name: string; firstLine: string }[]> {
    const entries: { name: string; firstLine: string }[] = [];
    for (const file of (await fs.readdir(this.memoryDir)).sort()) {
      if (!file.endsWith(".md")) {
        continue;
      }
      const contents = await fs.readFile(
        path.join(this.memoryDir, file),
        "utf8",
      );
      entries.push({
        name: file.slice(0, -3),
        firstLine: contents.split("\n", 1)[0].slice(0, 120),
      });
    }
    return entries;
  }

  // Loads agent-authored tool modules, wiring the emulator context into each
  // execute call. Invalid modules are skipped with a warning (the agent is
  // told at install time; this guards later manual edits).
  async loadAgentTools(
    context: AgentToolContext,
    warn: (message: string) => void,
  ): Promise<AgentTool[]> {
    const tools: AgentTool[] = [];
    for (const file of (await fs.readdir(this.toolsDir)).sort()) {
      if (!file.endsWith(".mjs")) {
        continue;
      }
      const filePath = path.join(this.toolsDir, file);
      try {
        const tool = await importToolModule(filePath);
        tools.push({
          name: tool.name,
          description: `${tool.description} (agent-authored tool)`,
          parameters: tool.parameters,
          attachFrameAfter: true,
          execute: async (args) => {
            const result = await tool.execute(args, context);
            return { text: stringifyToolReturn(result) };
          },
        });
      } catch (error) {
        warn(`skipping agent tool ${file}: ${errorMessage(error)}`);
      }
    }
    return tools;
  }
}

interface AgentToolModule {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  execute: (
    args: Record<string, unknown>,
    context: AgentToolContext,
  ) => Promise<unknown>;
}

async function importToolModule(filePath: string): Promise<AgentToolModule> {
  // Cache-busting query so reinstalls of the same name load fresh source.
  const url = `${pathToFileURL(filePath).href}?v=${Date.now()}`;
  const module = (await import(url)) as { default?: unknown };
  const tool = module.default as Partial<AgentToolModule> | undefined;
  if (
    tool === undefined ||
    typeof tool.name !== "string" ||
    !NAME_RE.test(tool.name) ||
    typeof tool.description !== "string" ||
    tool.parameters === null ||
    typeof tool.parameters !== "object" ||
    typeof tool.execute !== "function"
  ) {
    throw new Error(
      "default export must be {name (snake_case string), description, parameters (JSON schema object), execute(args, ctx)}",
    );
  }
  return tool as AgentToolModule;
}

function stringifyToolReturn(value: unknown): string {
  if (typeof value === "string") {
    return value.slice(0, 8_000);
  }
  return JSON.stringify(value ?? null).slice(0, 8_000);
}

export function selfTools(store: SelfStore): AgentTool[] {
  return [
    {
      name: "update_playbook",
      description:
        "Replace your strategy playbook. The playbook is injected into your prompt at the start of every turn — it is your primary self-improvement channel. Keep it organized: world map / current location notes, button timing lore, battle heuristics, menu navigation, mistakes to avoid.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          content: {
            type: "string",
            description: `Full new playbook markdown (max ${PLAYBOOK_MAX_CHARS} chars). Replaces the old playbook entirely, so carry forward anything still useful.`,
          },
        },
        required: ["content"],
      },
      execute: async (args): Promise<ToolResult> => {
        const content = String(args.content ?? "");
        if (content.length === 0 || content.length > PLAYBOOK_MAX_CHARS) {
          return {
            text: `playbook must be 1-${PLAYBOOK_MAX_CHARS} chars (got ${content.length})`,
          };
        }
        await fs.writeFile(store.playbookPath, content, "utf8");
        return {
          text: `playbook updated (${content.length} chars); it will be in your prompt from the next turn on`,
          improvement: "PLAYBOOK updated",
        };
      },
    },
    {
      name: "save_memory",
      description:
        "Save a named knowledge file that persists across turns: town layouts, NPC dialog, quest steps, verified type matchups. The memory index (names + first lines) is shown every turn; use read_memory for full contents. Make the first line a useful summary.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description: "snake_case name, e.g. 'pallet_town_map'.",
          },
          content: { type: "string", description: "Markdown contents." },
        },
        required: ["name", "content"],
      },
      execute: async (args): Promise<ToolResult> => {
        const name = String(args.name ?? "");
        const content = String(args.content ?? "");
        if (!NAME_RE.test(name)) {
          return { text: "name must match ^[a-z][a-z0-9_-]{0,63}$" };
        }
        if (content.length === 0 || content.length > MEMORY_MAX_CHARS) {
          return {
            text: `content must be 1-${MEMORY_MAX_CHARS} chars (got ${content.length})`,
          };
        }
        await fs.writeFile(
          path.join(store.memoryDir, `${name}.md`),
          content,
          "utf8",
        );
        return {
          text: `memory '${name}' saved`,
          improvement: `MEMORY: ${name}`,
        };
      },
    },
    {
      name: "read_memory",
      description: "Read the full contents of a saved memory file.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: { type: "string", description: "Memory name from the index." },
        },
        required: ["name"],
      },
      execute: async (args) => {
        const name = String(args.name ?? "");
        if (!NAME_RE.test(name)) {
          return { text: "name must match ^[a-z][a-z0-9_-]{0,63}$" };
        }
        try {
          return {
            text: await fs.readFile(
              path.join(store.memoryDir, `${name}.md`),
              "utf8",
            ),
          };
        } catch {
          return { text: `no memory named '${name}'` };
        }
      },
    },
    {
      name: "delete_memory",
      description: "Delete a saved memory file that is wrong or obsolete.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: { type: "string", description: "Memory name to delete." },
        },
        required: ["name"],
      },
      execute: async (args) => {
        const name = String(args.name ?? "");
        if (!NAME_RE.test(name)) {
          return { text: "name must match ^[a-z][a-z0-9_-]{0,63}$" };
        }
        try {
          await fs.unlink(path.join(store.memoryDir, `${name}.md`));
          return { text: `memory '${name}' deleted` };
        } catch {
          return { text: `no memory named '${name}'` };
        }
      },
    },
    {
      name: "update_todos",
      description:
        "Replace your persistent goal stack. It is shown at the start of every turn — this is how you hold long-horizon plans across turns that each only see one screen. Keep it short and ordered.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          todos: {
            type: "array",
            maxItems: 20,
            items: {
              type: "object",
              additionalProperties: false,
              properties: {
                text: { type: "string" },
                status: {
                  type: "string",
                  enum: ["pending", "in_progress", "done"],
                },
              },
              required: ["text", "status"],
            },
          },
        },
        required: ["todos"],
      },
      execute: async (args): Promise<ToolResult> => {
        const todos = Array.isArray(args.todos) ? args.todos : null;
        if (
          todos === null ||
          todos.length > 20 ||
          todos.some(
            (todo) =>
              typeof todo?.text !== "string" ||
              !["pending", "in_progress", "done"].includes(
                String(todo?.status),
              ),
          )
        ) {
          return {
            text: "todos must be <=20 items of {text, status: pending|in_progress|done}",
          };
        }
        await fs.writeFile(
          store.todosPath,
          `${JSON.stringify(todos, null, 2)}\n`,
          "utf8",
        );
        return {
          text: `todo list updated (${todos.length} items)`,
          improvement: "TODOS updated",
        };
      },
    },
    {
      name: "install_tool",
      description:
        "Author a new tool for yourself. Provide the source of an ES module whose default export is {name, description, parameters (JSON schema), async execute(args, ctx)}. ctx.emulator has: press(buttons, holdFrames?, waitFrames?), tick(frames), frame(), saveCheckpoint(name), loadCheckpoint(name) — all returning {state, screen_hash, ...}; ctx.log(msg) prints to the run log. The tool becomes callable on your NEXT round trip. Good candidates: movement macros (walk a path, mash through dialog), battle routines, lookups over your memory files.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description:
              "snake_case tool name; must equal the module's exported name.",
          },
          source: {
            type: "string",
            description: "Full ES module source code.",
          },
        },
        required: ["name", "source"],
      },
      execute: async (args): Promise<ToolResult> => {
        const name = String(args.name ?? "");
        const source = String(args.source ?? "");
        if (!NAME_RE.test(name)) {
          return { text: "name must match ^[a-z][a-z0-9_-]{0,63}$" };
        }
        if (RESERVED_TOOL_NAMES.has(name)) {
          return { text: `'${name}' is a built-in tool name; pick another` };
        }
        if (source.length === 0 || source.length > TOOL_SOURCE_MAX_CHARS) {
          return {
            text: `source must be 1-${TOOL_SOURCE_MAX_CHARS} chars (got ${source.length})`,
          };
        }
        const filePath = path.join(store.toolsDir, `${name}.mjs`);
        await fs.writeFile(filePath, source, "utf8");
        try {
          const tool = await importToolModule(filePath);
          if (tool.name !== name) {
            throw new Error(
              `module exports name '${tool.name}' but the file is '${name}'`,
            );
          }
        } catch (error) {
          await fs.unlink(filePath).catch(() => {});
          return {
            text: `tool rejected, nothing installed: ${errorMessage(error)}`,
          };
        }
        return {
          text: `tool '${name}' installed; callable from your next round trip`,
          improvement: `NEW TOOL: ${name}`,
        };
      },
    },
    {
      name: "uninstall_tool",
      description: "Remove one of your agent-authored tools.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: { type: "string", description: "Tool name to remove." },
        },
        required: ["name"],
      },
      execute: async (args) => {
        const name = String(args.name ?? "");
        if (!NAME_RE.test(name)) {
          return { text: "name must match ^[a-z][a-z0-9_-]{0,63}$" };
        }
        try {
          await fs.unlink(path.join(store.toolsDir, `${name}.mjs`));
          return { text: `tool '${name}' uninstalled` };
        } catch {
          return { text: `no agent tool named '${name}'` };
        }
      },
    },
  ];
}

const RESERVED_TOOL_NAMES = new Set([
  "press_buttons",
  "wait",
  "screenshot",
  "save_checkpoint",
  "load_checkpoint",
  "list_checkpoints",
  "update_playbook",
  "save_memory",
  "read_memory",
  "delete_memory",
  "update_todos",
  "install_tool",
  "uninstall_tool",
]);

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
