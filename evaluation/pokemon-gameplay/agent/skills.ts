// Skills for the pokemon harness: the same agent-skills standard and tool
// surface as @exo/harness skill-tools (install_skill / use_skill /
// read_skill_file / uninstall_skill, progressive disclosure), but backed by
// the run directory instead of agent artifacts — this harness runs outside
// the exo host. Storage is the standard on-disk layout, so skills here are
// portable to any agent-skills consumer:
//
//   <runtime>/skills/<name>/SKILL.md
//   <runtime>/skills/<name>/<bundled files>
//
// Frontmatter parsing is shared with the harness implementation.

import fs from "node:fs/promises";
import path from "node:path";

import { parseSkillFrontmatter } from "../../../typescript/harness/skill-tools";

import type { AgentTool, ToolResult } from "./tool-types";

const SKILL_NAME_PATTERN = /^[a-z0-9]+(-[a-z0-9]+)*$/;
const MAX_NAME_CHARS = 64;
const MAX_DESCRIPTION_CHARS = 1024;
const MAX_SKILL_MD_CHARS = 100_000;
const MAX_FILE_CHARS = 200_000;
const MAX_FILES = 64;

export interface SkillIndexEntry {
  name: string;
  description: string;
}

export class SkillsStore {
  readonly skillsDir: string;

  constructor(runtimeDir: string) {
    this.skillsDir = path.join(runtimeDir, "skills");
  }

  async init(): Promise<void> {
    await fs.mkdir(this.skillsDir, { recursive: true });
  }

  // Stage 1 of progressive disclosure: names + descriptions only, injected
  // into the prompt every turn.
  async index(): Promise<SkillIndexEntry[]> {
    const entries: SkillIndexEntry[] = [];
    for (const dir of (await fs.readdir(this.skillsDir)).sort()) {
      const skillMd = await this.readSkillMd(dir);
      if (skillMd === null) {
        continue;
      }
      const parsed = parseSkillFrontmatter(skillMd);
      const description = parsed?.fields.description;
      if (typeof description === "string" && description.length > 0) {
        entries.push({ name: dir, description });
      }
    }
    return entries;
  }

  async readSkillMd(name: string): Promise<string | null> {
    try {
      return await fs.readFile(
        path.join(this.skillsDir, name, "SKILL.md"),
        "utf8",
      );
    } catch {
      return null;
    }
  }

  async listFiles(name: string): Promise<string[]> {
    const root = path.join(this.skillsDir, name);
    const found: string[] = [];
    const walk = async (dir: string): Promise<void> => {
      for (const entry of await fs.readdir(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
          await walk(full);
        } else if (path.relative(root, full) !== "SKILL.md") {
          found.push(path.relative(root, full));
        }
      }
    };
    try {
      await walk(root);
    } catch {
      return [];
    }
    return found.sort();
  }

  // Resolves a bundled-file path, refusing traversal outside the skill dir.
  resolveFile(name: string, relative: string): string | null {
    const root = path.join(this.skillsDir, name);
    const full = path.resolve(root, relative);
    if (full !== root && !full.startsWith(`${root}${path.sep}`)) {
      return null;
    }
    return full;
  }
}

function validateName(name: string): string | null {
  if (
    name.length === 0 ||
    name.length > MAX_NAME_CHARS ||
    !SKILL_NAME_PATTERN.test(name)
  ) {
    return "skill name must be 1-64 chars of lowercase letters/digits with single hyphens";
  }
  return null;
}

export function skillTools(store: SkillsStore): AgentTool[] {
  return [
    {
      name: "install_skill",
      description:
        "Install (or update) a durable skill: a reusable, named procedure you can reload in any future turn. A skill is a SKILL.md in the standard agent-skills format — YAML frontmatter with required name and description, then a markdown body of instructions — plus optional bundled text files. Only the name + description appear in your prompt each turn; load the body with use_skill when it applies. Use skills for procedures bigger than a playbook line: how to win a wild battle, how to navigate a maze area, how to shop efficiently. Installing an existing name updates it.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          skillMd: {
            type: "string",
            description:
              "Full SKILL.md contents. Must start with YAML frontmatter: ---\\nname: my-skill\\ndescription: What it does and when to use it.\\n---\\nfollowed by markdown instructions.",
          },
          files: {
            type: ["array", "null"],
            description:
              "Supporting text files bundled with the skill, or null for none.",
            items: {
              type: "object",
              additionalProperties: false,
              properties: {
                path: {
                  type: "string",
                  description:
                    "Relative path within the skill, e.g. notes/map.md.",
                },
                contents: {
                  type: "string",
                  description: "UTF-8 text contents.",
                },
              },
              required: ["path", "contents"],
            },
          },
        },
        required: ["skillMd", "files"],
      },
      execute: async (args): Promise<ToolResult> => {
        const skillMd = typeof args.skillMd === "string" ? args.skillMd : "";
        if (
          skillMd.trim().length === 0 ||
          skillMd.length > MAX_SKILL_MD_CHARS
        ) {
          return {
            text: `skillMd is required (max ${MAX_SKILL_MD_CHARS} chars)`,
          };
        }
        const parsed = parseSkillFrontmatter(skillMd);
        if (parsed === null) {
          return {
            text: "skillMd must start with YAML frontmatter delimited by --- lines",
          };
        }
        const name = parsed.fields.name ?? "";
        const nameError = validateName(name);
        if (nameError !== null) {
          return { text: nameError };
        }
        const description = parsed.fields.description ?? "";
        if (
          description.length === 0 ||
          description.length > MAX_DESCRIPTION_CHARS
        ) {
          return {
            text: `frontmatter description must be 1-${MAX_DESCRIPTION_CHARS} chars: what the skill does and when to use it`,
          };
        }
        const files = Array.isArray(args.files) ? args.files : [];
        if (files.length > MAX_FILES) {
          return { text: `too many files (max ${MAX_FILES})` };
        }
        const skillDir = path.join(store.skillsDir, name);
        const wanted: { full: string; contents: string }[] = [];
        for (const file of files) {
          const rel = typeof file?.path === "string" ? file.path : "";
          const contents =
            typeof file?.contents === "string" ? file.contents : "";
          const full = store.resolveFile(name, rel);
          if (rel.length === 0 || full === null || rel === "SKILL.md") {
            return { text: `invalid file path: ${rel}` };
          }
          if (contents.length > MAX_FILE_CHARS) {
            return { text: `file ${rel} exceeds ${MAX_FILE_CHARS} chars` };
          }
          wanted.push({ full, contents });
        }
        const existed = (await store.readSkillMd(name)) !== null;
        // Replace wholesale so an update cannot leave stale bundled files.
        await fs.rm(skillDir, { recursive: true, force: true });
        await fs.mkdir(skillDir, { recursive: true });
        await fs.writeFile(path.join(skillDir, "SKILL.md"), skillMd, "utf8");
        for (const file of wanted) {
          await fs.mkdir(path.dirname(file.full), { recursive: true });
          await fs.writeFile(file.full, file.contents, "utf8");
        }
        return {
          text: `skill '${name}' ${existed ? "updated" : "installed"} (${wanted.length} bundled files); it is listed in your prompt from now on`,
          improvement: `${existed ? "SKILL updated" : "NEW SKILL"}: ${name}`,
        };
      },
    },
    {
      name: "use_skill",
      description:
        "Load a skill's full SKILL.md instructions plus the paths of its bundled files. Call this before a task that matches an installed skill's description, then follow the instructions.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: {
            type: "string",
            description: "Skill name from your prompt's skill list.",
          },
        },
        required: ["name"],
      },
      execute: async (args): Promise<ToolResult> => {
        const name = typeof args.name === "string" ? args.name.trim() : "";
        if (validateName(name) !== null) {
          return { text: "unknown skill name" };
        }
        const skillMd = await store.readSkillMd(name);
        if (skillMd === null) {
          return { text: `skill not installed: ${name}` };
        }
        const files = await store.listFiles(name);
        return {
          text:
            `${skillMd}\n\n` +
            (files.length > 0
              ? `Bundled files (read with read_skill_file): ${files.join(", ")}`
              : "(no bundled files)"),
        };
      },
    },
    {
      name: "read_skill_file",
      description:
        "Read one supporting file bundled with an installed skill, by the relative path listed in the use_skill result.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: { type: "string", description: "The skill name." },
          path: {
            type: "string",
            description: "Relative file path within the skill.",
          },
        },
        required: ["name", "path"],
      },
      execute: async (args): Promise<ToolResult> => {
        const name = typeof args.name === "string" ? args.name.trim() : "";
        const rel = typeof args.path === "string" ? args.path.trim() : "";
        if (validateName(name) !== null) {
          return { text: "unknown skill name" };
        }
        const full = store.resolveFile(name, rel);
        if (full === null) {
          return { text: `invalid file path: ${rel}` };
        }
        try {
          return { text: await fs.readFile(full, "utf8") };
        } catch {
          const files = await store.listFiles(name);
          return {
            text: `no file ${rel} in skill ${name}; bundled files: ${files.join(", ") || "(none)"}`,
          };
        }
      },
    },
    {
      name: "uninstall_skill",
      description: "Remove an installed skill by name.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          name: { type: "string", description: "The skill name to remove." },
        },
        required: ["name"],
      },
      execute: async (args): Promise<ToolResult> => {
        const name = typeof args.name === "string" ? args.name.trim() : "";
        if (validateName(name) !== null) {
          return { text: "unknown skill name" };
        }
        if ((await store.readSkillMd(name)) === null) {
          return { text: `skill not installed: ${name}` };
        }
        await fs.rm(path.join(store.skillsDir, name), {
          recursive: true,
          force: true,
        });
        return {
          text: `skill '${name}' uninstalled`,
          improvement: `SKILL removed: ${name}`,
        };
      },
    },
  ];
}
