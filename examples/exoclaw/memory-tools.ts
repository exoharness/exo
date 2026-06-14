import { randomUUID } from "node:crypto";

import { z } from "zod";

import type {
  Agent,
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  Message,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

// Durable agent-writable memory: a single global store that persists across
// every conversation for this agent. Stored as a JSON artifact on the agent
// handle, mirroring how config/executor.json is persisted. See
// examples/exoclaw/docs/SELF-CONTROL.md section 2 for the design.
const MEMORY_ARTIFACT_PATH = "memory/exoclaw-memory.json";
// Soft caps so always-injecting memory cannot grow the prompt without bound.
const MAX_ENTRIES = 200;
const MAX_TEXT_CHARS = 600;

const MemoryEntrySchema = z.object({
  id: z.string(),
  text: z.string(),
  createdAt: z.string(),
});

const MemoryStoreSchema = z.object({
  entries: z.array(MemoryEntrySchema),
});

type MemoryEntry = z.infer<typeof MemoryEntrySchema>;
type MemoryStore = z.infer<typeof MemoryStoreSchema>;

// The artifact subset both Agent and the test fake provide. readLatestArtifact
// pushes the path filter + latest-version sort into exo (see the trait defaults
// in crates/exoharness) rather than listing every artifact and filtering here.
type MemoryHandle = Pick<Agent, "readLatestArtifactJson" | "writeArtifactJson">;

function memoryHandle(context: TurnContext): MemoryHandle {
  return context.exoharness.current.agent;
}

async function readMemory(handle: MemoryHandle): Promise<MemoryStore> {
  const raw = await handle.readLatestArtifactJson({
    path: MEMORY_ARTIFACT_PATH,
  });
  if (raw === null) {
    // Nothing has ever been written — a legitimately empty store.
    return { entries: [] };
  }
  // The artifact exists, so an invalid payload is a real error. Surface it
  // loudly instead of masking it as "no memory" — otherwise the next write
  // would silently bury a corrupt store.
  const parsed = MemoryStoreSchema.safeParse(raw);
  if (!parsed.success) {
    const message = `corrupt memory artifact ${MEMORY_ARTIFACT_PATH}: ${parsed.error.message}`;
    console.error(message);
    throw new Error(message);
  }
  return parsed.data;
}

async function writeMemory(
  handle: MemoryHandle,
  store: MemoryStore,
): Promise<void> {
  await handle.writeArtifactJson({
    path: MEMORY_ARTIFACT_PATH,
    value: store as unknown as JsonValue,
  });
}

function rememberTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "remember",
      description:
        "Save a durable fact so you still have it in future turns and conversations. Use this when the user shares a lasting preference or fact about themselves, or when you want to persist something about yourself. The fact is injected into your context every turn. Phrase it as a standalone statement.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          text: {
            type: "string",
            description:
              'The fact to remember, phrased to stand on its own (e.g. "User\'s favorite coffee is a flat white").',
          },
        },
        required: ["text"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const text = typeof args.text === "string" ? args.text.trim() : "";
        if (text.length === 0) {
          return { ok: false, error: "text is required" };
        }
        if (text.length > MAX_TEXT_CHARS) {
          return {
            ok: false,
            error: `text exceeds ${MAX_TEXT_CHARS} characters; summarize it first`,
          };
        }
        const handle = memoryHandle(execution.context);
        const store = await readMemory(handle);
        const entry: MemoryEntry = {
          id: `mem_${randomUUID().slice(0, 8)}`,
          text,
          createdAt: new Date().toISOString(),
        };
        store.entries.push(entry);
        let dropped = 0;
        if (store.entries.length > MAX_ENTRIES) {
          dropped = store.entries.length - MAX_ENTRIES;
          store.entries = store.entries.slice(-MAX_ENTRIES);
        }
        await writeMemory(handle, store);
        return { ok: true, id: entry.id, total: store.entries.length, dropped };
      },
    },
  };
}

function forgetTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "forget",
      description:
        "Remove a previously saved memory by its id. Memory ids are shown in brackets in the durable-memory block in your context.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          id: {
            type: "string",
            description: "The memory id to remove, e.g. mem_1a2b3c4d.",
          },
        },
        required: ["id"],
      },
    },
    handler: {
      async execute(args: JsonObject, execution): Promise<ToolResult> {
        const id = typeof args.id === "string" ? args.id.trim() : "";
        if (id.length === 0) {
          return { ok: false, error: "id is required" };
        }
        const handle = memoryHandle(execution.context);
        const store = await readMemory(handle);
        const before = store.entries.length;
        store.entries = store.entries.filter((entry) => entry.id !== id);
        const removed = before - store.entries.length;
        if (removed > 0) {
          await writeMemory(handle, store);
        }
        return { ok: removed > 0, id, removed };
      },
    },
  };
}

export function createMemoryToolInstances(): ToolInstance[] {
  return [rememberTool(), forgetTool()];
}

export function registerMemoryTools(registry: HarnessToolRegistry): void {
  for (const tool of createMemoryToolInstances()) {
    registry.register(tool);
  }
}

// Build the developer message that injects saved memory into the prompt.
// Returns null when nothing has been remembered yet.
export async function memoryInstruction(
  context: TurnContext,
): Promise<Message | null> {
  const store = await readMemory(memoryHandle(context));
  if (store.entries.length === 0) {
    return null;
  }
  const lines = store.entries.map((entry) => `- [${entry.id}] ${entry.text}`);
  return {
    role: "developer",
    content: `Durable memory you saved earlier with the remember tool. Treat these as authoritative context. Add new facts with remember and drop stale ones with forget(id).\n\n${lines.join("\n")}`,
  };
}
