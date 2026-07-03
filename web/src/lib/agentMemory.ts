import type { ExoClient } from "../api/exoClient";
import type { AgentId } from "../api/protocol";

// Upstream exoclaw stores agent-writable durable memory (the remember/forget
// tools, exo PR #62/#70) as a single JSON artifact on the agent handle — no
// substrate change. We surface it read-only. The path + shape are fixed by the
// harness; this is exoclaw-specific, so absence/invalidity degrades to "none".
const MEMORY_ARTIFACT_PATH = "memory/exoclaw-memory.json";

export interface MemoryEntry {
  id: string;
  text: string;
  createdAt: string;
}

function decodeContents(contents: number[]): string {
  return new TextDecoder().decode(Uint8Array.from(contents));
}

function isMemoryEntry(value: unknown): value is MemoryEntry {
  if (!value || typeof value !== "object") {
    return false;
  }
  const entry = value as Record<string, unknown>;
  return (
    typeof entry.id === "string" &&
    typeof entry.text === "string" &&
    typeof entry.createdAt === "string"
  );
}

// Returns the agent's memory entries, or null when the agent has no memory
// artifact (or it is unreadable) — callers render nothing in that case.
export async function loadAgentMemory(
  client: ExoClient,
  agentId: AgentId,
): Promise<MemoryEntry[] | null> {
  const artifacts = await client.listAgentArtifacts(agentId);
  const ref = artifacts.find(
    (artifact) => artifact.path === MEMORY_ARTIFACT_PATH,
  );
  if (!ref) {
    return null;
  }

  const artifact = await client.readAgentArtifact(
    agentId,
    ref.artifact_id,
    ref.version,
  );

  let parsed: unknown;
  try {
    parsed = JSON.parse(decodeContents(artifact.contents));
  } catch {
    return null;
  }

  if (
    !parsed ||
    typeof parsed !== "object" ||
    !Array.isArray((parsed as { entries?: unknown }).entries)
  ) {
    return null;
  }

  return (parsed as { entries: unknown[] }).entries.filter(isMemoryEntry);
}
