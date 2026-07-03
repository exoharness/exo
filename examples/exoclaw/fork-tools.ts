import { execFile, spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { closeSync, mkdirSync, openSync } from "node:fs";
import { chmod, mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

import type {
  Agent,
  ArtifactVersion,
  Conversation,
  EventData,
  HarnessToolRegistry,
  JsonObject,
  JsonValue,
  Message,
  ToolInstance,
  ToolResult,
  TurnContext,
} from "@exo/harness";

const execFileAsync = promisify(execFile);
const ROOT_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../..",
);
const EXO_BIN =
  process.env.EXO_BIN ?? path.join(ROOT_DIR, "target", "debug", "exo");
const TRIBE_ROOT = path.join(ROOT_DIR, ".exo", ".tribe");
const FAMILY_ARTIFACT_PATH = "fork/family.json";
const FAMILY_REF_ARTIFACT_PATH = "fork/family-ref.json";
const MEMORY_ARTIFACT_PATH = "memory/exoclaw-memory.json";
const AGENT_CONFIG_ARTIFACT_PATH = "config/executor.json";
const CONVERSATION_CONFIG_ARTIFACT_PATH = "config/executor.json";

type ForkStatus = "active" | "terminating" | "killed";

interface FamilyRef {
  familyId: string;
  rootAgentId: string;
}

interface FamilyAgentRecord {
  agentId: string;
  slug: string;
  name: string;
  parentAgentId: string | null;
  generation: number;
  status: ForkStatus;
  purpose: string | null;
  nodePath: string;
  sourceRoot: string;
  conversationId: string | null;
  conversationSlug: string | null;
  createdAt: string;
}

type FamilyEvent =
  | {
      id: string;
      createdAt: string;
      type: "agent_forked";
      actorAgentId: string;
      parentAgentId: string;
      childAgentId: string;
      childSlug: string;
      childName: string;
      purpose: string | null;
      sourceConversationId: string;
      sourceTurnId: string;
    }
  | {
      id: string;
      createdAt: string;
      type: "agent_kill_requested";
      actorAgentId: string;
      parentAgentId: string;
      childAgentId: string;
      reason: string;
    }
  | {
      id: string;
      createdAt: string;
      type: "agent_killed";
      actorAgentId: string;
      childAgentId: string;
      reason: string;
      deleteState: boolean;
    }
  | {
      id: string;
      createdAt: string;
      type: "fork_message_sent";
      actorAgentId: string;
      fromAgentId: string;
      toAgentId: string;
      toConversationId: string;
      message: string;
      expectsReply: boolean;
    };

// TODO(storage-rework): the family store is read-modify-write on the root
// agent's artifact with no compare-and-swap, and unlike agent memory it is
// written by multiple agents (parent and children). Concurrent fork/kill/
// message calls can lose updates. Fix alongside the artifact versioning
// rework, likely by appending one event file per write (the .exo/.tribe
// events/ directory is already shaped for that) instead of rewriting the
// whole store.
interface FamilyStore {
  familyId: string;
  rootAgentId: string;
  rootAgentSlug: string;
  createdAt: string;
  agents: FamilyAgentRecord[];
  events: FamilyEvent[];
}

interface FamilyContext {
  family: FamilyStore;
  rootAgent: Agent;
}

export function registerForkTools(registry: HarnessToolRegistry): void {
  for (const tool of createForkToolInstances()) {
    registry.register(tool);
  }
}

export function createForkToolInstances(): ToolInstance[] {
  return [
    forkTool(),
    killForkTool(),
    listForksTool(),
    listForkEventsTool(),
    sendForkMessageTool(),
  ];
}

function forkTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "fork",
      description:
        "Create a child Exo agent with its own conversation, source worktree, sandbox lineage node, and shared family ledger. Use this for delegated or experimental work that should not mutate the parent directly.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          slug: {
            type: ["string", "null"],
            description:
              "Optional globally unique child agent slug. Null derives one from the parent and purpose.",
          },
          name: {
            type: ["string", "null"],
            description:
              "Optional child display name. Null derives one from the parent and purpose.",
          },
          purpose: {
            type: ["string", "null"],
            description: "Short reason this child is being created.",
          },
          initialPrompt: {
            type: ["string", "null"],
            description:
              "Optional message to seed into the child conversation as its first assignment.",
          },
          conversationSlug: {
            type: ["string", "null"],
            description:
              "Optional initial child conversation slug. Null uses dev.",
          },
          conversationName: {
            type: ["string", "null"],
            description:
              "Optional initial child conversation name. Null uses Dev.",
          },
          inheritMemory: {
            type: ["boolean", "null"],
            description:
              "Whether to copy the parent's durable memory artifact. Null defaults to true.",
          },
          sandbox: {
            type: ["string", "null"],
            enum: ["fresh", null],
            description:
              "Child sandbox strategy. Only fresh is implemented in this first slice.",
          },
          adapters: {
            type: ["string", "null"],
            enum: ["none", null],
            description:
              "Child adapter strategy. Only none is implemented in this first slice.",
          },
        },
        required: [
          "slug",
          "name",
          "purpose",
          "initialPrompt",
          "conversationSlug",
          "conversationName",
          "inheritMemory",
          "sandbox",
          "adapters",
        ],
      },
    },
    handler: {
      execute(args, execution) {
        return executeForkTool(args, execution.context);
      },
    },
  };
}

function killForkTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "kill_fork",
      description:
        "Mark a descendant fork as killed, cascading to all of its descendants. By default this preserves child state for inspection; set deleteState true only for explicit hard deletion, which also deletes the subtree's agents, source worktrees, and lineage directories.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          childAgentId: {
            type: "string",
            description: "Child agent id or slug.",
          },
          reason: {
            type: "string",
            description: "Reason the child should be stopped.",
          },
          deleteState: {
            type: ["boolean", "null"],
            description:
              "Whether to delete the child agent state. Null/false preserves state.",
          },
        },
        required: ["childAgentId", "reason", "deleteState"],
      },
    },
    handler: {
      execute(args, execution) {
        return executeKillForkTool(args, execution.context);
      },
    },
  };
}

function listForksTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "list_forks",
      description:
        "List agents in this fork family, including parent/child lineage, status, purpose, node path, and source root.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          includeKilled: {
            type: ["boolean", "null"],
            description:
              "Whether to include killed children. Null defaults to false.",
          },
        },
        required: ["includeKilled"],
      },
    },
    handler: {
      execute(args, execution) {
        return executeListForksTool(args, execution.context);
      },
    },
  };
}

function listForkEventsTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "list_fork_events",
      description:
        "List recent shared family ledger events for this agent's fork family.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          limit: {
            type: ["number", "null"],
            description: "Maximum events to return. Null defaults to 50.",
          },
        },
        required: ["limit"],
      },
    },
    handler: {
      execute(args, execution) {
        return executeListForkEventsTool(args, execution.context);
      },
    },
  };
}

function sendForkMessageTool(): ToolInstance {
  return {
    source: "built_in",
    definition: {
      name: "send_fork_message",
      description:
        "Send an internal coordination message to a parent or child agent in the same fork family. This records the message in the shared family ledger, then delivers it as a user message to the target conversation by starting a detached turn on the target agent.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          targetAgentId: {
            type: "string",
            description: "Target agent id or slug in this fork family.",
          },
          message: {
            type: "string",
            description: "Message to send.",
          },
          conversationSlug: {
            type: ["string", "null"],
            description:
              "Optional target conversation slug or id. Null uses the target's default fork conversation.",
          },
          expectsReply: {
            type: ["boolean", "null"],
            description:
              "Whether the sender expects a reply. Null defaults to true.",
          },
        },
        required: [
          "targetAgentId",
          "message",
          "conversationSlug",
          "expectsReply",
        ],
      },
    },
    handler: {
      execute(args, execution) {
        return executeSendForkMessageTool(args, execution.context);
      },
    },
  };
}

async function executeForkTool(
  args: JsonObject,
  context: TurnContext,
): Promise<ToolResult> {
  if (stringOrNull(args.sandbox) !== null && args.sandbox !== "fresh") {
    return { ok: false, error: "only sandbox='fresh' is implemented" };
  }
  if (stringOrNull(args.adapters) !== null && args.adapters !== "none") {
    return { ok: false, error: "only adapters='none' is implemented" };
  }

  const familyContext = await loadOrCreateFamily(context);
  const { family, rootAgent } = familyContext;
  const parent = currentFamilyAgent(family, context);
  if (parent.status === "killed") {
    return { ok: false, error: "killed agents cannot create forks" };
  }

  const purpose = stringOrNull(args.purpose);
  const childIndex = nextChildIndex(family, parent.agentId);
  const nodeName = nodeNameFor(stringOrNull(args.slug), purpose, childIndex);
  const childSlug = uniqueChildSlug(
    stringOrNull(args.slug) ??
      `${context.exoharness.current.agent.record.slug}-${nodeName}`,
    await context.exoharness.listAgents(),
  );
  const label = purpose ? titleize(purpose) : `Fork ${childIndex}`;
  const childName =
    stringOrNull(args.name) ??
    `${context.exoharness.current.agent.record.name} / ${label}`;
  const conversationSlug = stringOrNull(args.conversationSlug) ?? "dev";
  const conversationName = stringOrNull(args.conversationName) ?? "Dev";
  const now = new Date().toISOString();
  const nodePath = `${parent.nodePath}/children/${nodeName}`;
  const sourceRoot = path.join(
    TRIBE_ROOT,
    family.rootAgentSlug,
    nodePath,
    "repo",
  );

  const child = await context.exoharness.newAgent({
    slug: childSlug,
    name: childName,
  });
  // Every step after agent creation can fail (artifact writes, worktree
  // collisions, ...). Roll the child back on failure so retries reuse the
  // same slug instead of accumulating orphaned agents with -N suffixes.
  try {
    await child.writeArtifactJson({
      path: AGENT_CONFIG_ARTIFACT_PATH,
      value: rustAgentConfig(context) as JsonValue,
    });
    await writeFamilyRef(child, {
      familyId: family.familyId,
      rootAgentId: family.rootAgentId,
    });

    const conversation = await child.newConversation({
      slug: conversationSlug,
      name: conversationName,
    });
    await conversation.writeArtifactJson({
      path: CONVERSATION_CONFIG_ARTIFACT_PATH,
      value: rustConversationConfig(context, {
        fromSourceRoot: parent.sourceRoot,
        toSourceRoot: sourceRoot,
      }) as JsonValue,
    });

    if (args.inheritMemory !== false) {
      await copyLatestJsonArtifact(
        context.exoharness.current.agent,
        child,
        MEMORY_ARTIFACT_PATH,
      );
    }

    const childRecord: FamilyAgentRecord = {
      agentId: child.record.id,
      slug: child.record.slug,
      name: child.record.name,
      parentAgentId: parent.agentId,
      generation: parent.generation + 1,
      status: "active",
      purpose,
      nodePath,
      sourceRoot,
      conversationId: conversation.record.id,
      conversationSlug: conversation.record.slug,
      createdAt: now,
    };
    // Record the fork in the child's own append-only event log. The family
    // ledger is an agent-writable artifact; this event is the tamper-evident
    // canonical record that the child was cloned, preserved in its history even
    // if ledger artifacts are rewritten.
    const birthEvent: EventData = {
      type: "custom",
      event_type: "fork_birth",
      payload: {
        familyId: family.familyId,
        parentAgentId: parent.agentId,
        parentSlug: parent.slug,
        parentName: parent.name,
        childAgentId: child.record.id,
        generation: childRecord.generation,
        purpose,
        nodePath,
        sourceRoot,
        sourceConversationId: context.exoharness.current.conversation.record.id,
        sourceTurnId: context.exoharness.current.turn.record.id,
        createdAt: now,
      },
    };
    await conversation.addEvents({ data: [birthEvent] });
    family.agents.push(childRecord);
    family.events.push({
      id: eventId(),
      createdAt: now,
      type: "agent_forked",
      actorAgentId: context.exoharness.current.agent.record.id,
      parentAgentId: parent.agentId,
      childAgentId: child.record.id,
      childSlug: child.record.slug,
      childName: child.record.name,
      purpose,
      sourceConversationId: context.exoharness.current.conversation.record.id,
      sourceTurnId: context.exoharness.current.turn.record.id,
    });

    await materializeTribeNode(family, childRecord);
    await createSourceClone(parent.sourceRoot, sourceRoot, childSlug);
    await writeManageScript(family, childRecord);
    await saveFamily(rootAgent, family);

    const initialPrompt = stringOrNull(args.initialPrompt);
    let delivery: { pid: number | null; logPath: string } | null = null;
    if (initialPrompt) {
      delivery = deliverForkMessage({
        family,
        target: childRecord,
        conversationSlug: conversation.record.slug,
        fromAgentId: parent.agentId,
        fromAgentName: context.exoharness.current.agent.record.name,
        message: initialPrompt,
        expectsReply: true,
        sandboxProvider:
          context.conversationConfig.sandboxProvider ??
          context.agentConfig.sandboxProvider,
      });
    }

    return {
      ok: true,
      familyId: family.familyId,
      parentAgentId: parent.agentId,
      childAgentId: child.record.id,
      childSlug: child.record.slug,
      childName: child.record.name,
      conversationId: conversation.record.id,
      conversationSlug: conversation.record.slug,
      nodePath,
      sourceRoot,
      status: "active",
      initialPromptDelivery: delivery
        ? {
            mode: "detached_turn",
            pid: delivery.pid,
            logPath: delivery.logPath,
          }
        : null,
    };
  } catch (error) {
    const rollback = await rollbackFork(context, {
      childAgentId: child.record.id,
      sourceRoot,
    });
    return {
      ok: false,
      error: `fork failed and was rolled back: ${
        error instanceof Error ? error.message : String(error)
      }`,
      rollback: rollback as unknown as JsonValue,
    };
  }
}

// Best-effort teardown of a partially created fork so a failed fork leaves no
// trace: remove the tribe node directory (which contains the child's cloned
// repo, if it got that far) and delete the child agent record.
async function rollbackFork(
  context: TurnContext,
  partial: {
    childAgentId: string;
    sourceRoot: string;
  },
): Promise<string[]> {
  const notes: string[] = [];
  try {
    await rm(path.dirname(partial.sourceRoot), {
      recursive: true,
      force: true,
    });
  } catch (error) {
    notes.push(
      `failed to remove tribe node: ${
        error instanceof Error ? error.message : String(error)
      }`,
    );
  }
  try {
    await context.exoharness.deleteAgent(partial.childAgentId);
  } catch (error) {
    notes.push(
      `failed to delete child agent ${partial.childAgentId}: ${
        error instanceof Error ? error.message : String(error)
      }`,
    );
  }
  return notes;
}

async function executeKillForkTool(
  args: JsonObject,
  context: TurnContext,
): Promise<ToolResult> {
  const childRef = stringArgument(args, "childAgentId");
  const reason = stringArgument(args, "reason");
  const deleteState = args.deleteState === true;
  const familyContext = await loadOrCreateFamily(context);
  const { family, rootAgent } = familyContext;
  const actor = currentFamilyAgent(family, context);
  const child = resolveFamilyAgent(family, childRef);
  if (!child) {
    return { ok: false, error: `fork not found: ${childRef}` };
  }
  if (!isDescendantOf(family, child.agentId, actor.agentId)) {
    return {
      ok: false,
      error: "agents may only kill descendants in their fork family",
    };
  }
  if (child.status === "killed" && !deleteState) {
    return { ok: false, error: `fork is already killed: ${childRef}` };
  }
  const now = new Date().toISOString();
  family.events.push({
    id: eventId(),
    createdAt: now,
    type: "agent_kill_requested",
    actorAgentId: actor.agentId,
    parentAgentId: actor.agentId,
    childAgentId: child.agentId,
    reason,
  });
  // Killing a fork kills its whole subtree: descendants of a dead agent must
  // not stay active with a severed lineage.
  const subtree = collectSubtree(family, child.agentId);
  const killedSlugs: string[] = [];
  for (const member of subtree) {
    if (member.status === "killed") {
      continue;
    }
    member.status = "killed";
    killedSlugs.push(member.slug);
    family.events.push({
      id: eventId(),
      createdAt: new Date().toISOString(),
      type: "agent_killed",
      actorAgentId: actor.agentId,
      childAgentId: member.agentId,
      reason:
        member.agentId === child.agentId
          ? reason
          : `cascaded from kill of ${child.slug}: ${reason}`,
      deleteState,
    });
    await materializeTribeNode(family, member);
  }
  await saveFamily(rootAgent, family);
  const cleanupNotes: string[] = [];
  if (deleteState) {
    // Child repos are standalone clones, so removing the tribe node
    // directory (which contains each descendant's repo) is the whole git
    // cleanup. Agent records are deleted individually first.
    for (const member of [...subtree].reverse()) {
      await context.exoharness.deleteAgent(member.agentId);
    }
    await rm(tribeNodeDir(family, child), { recursive: true, force: true });
  }
  return {
    ok: true,
    childAgentId: child.agentId,
    killed: killedSlugs as unknown as JsonValue,
    deleteState,
    cleanup: cleanupNotes as unknown as JsonValue,
    note: deleteState
      ? "subtree agents, repos, and lineage directories were deleted"
      : "subtree was marked killed; runtime enforcement will skip future work",
  };
}

// Returns the subtree rooted at the given agent (inclusive), shallow first.
function collectSubtree(
  family: FamilyStore,
  rootAgentId: string,
): FamilyAgentRecord[] {
  const root = family.agents.find((agent) => agent.agentId === rootAgentId);
  if (!root) {
    return [];
  }
  const subtree: FamilyAgentRecord[] = [root];
  for (let index = 0; index < subtree.length; index += 1) {
    const parentId = subtree[index].agentId;
    for (const agent of family.agents) {
      if (agent.parentAgentId === parentId) {
        subtree.push(agent);
      }
    }
  }
  return subtree;
}

function gitErrorMessage(error: unknown): string {
  const stderr = (error as { stderr?: string }).stderr?.trim();
  if (stderr) {
    return stderr;
  }
  return error instanceof Error ? error.message : String(error);
}

async function executeListForksTool(
  args: JsonObject,
  context: TurnContext,
): Promise<ToolResult> {
  const includeKilled = args.includeKilled === true;
  const { family } = await loadOrCreateFamily(context);
  return {
    ok: true,
    familyId: family.familyId,
    rootAgentId: family.rootAgentId,
    forks: family.agents.filter(
      (agent) => includeKilled || agent.status !== "killed",
    ) as unknown as JsonValue,
  };
}

async function executeListForkEventsTool(
  args: JsonObject,
  context: TurnContext,
): Promise<ToolResult> {
  const rawLimit = numberOrNull(args.limit);
  const limit = rawLimit === null ? 50 : Math.max(0, Math.min(200, rawLimit));
  const { family } = await loadOrCreateFamily(context);
  return {
    ok: true,
    familyId: family.familyId,
    events: family.events.slice(-limit).reverse() as unknown as JsonValue,
  };
}

async function executeSendForkMessageTool(
  args: JsonObject,
  context: TurnContext,
): Promise<ToolResult> {
  const targetRef = stringArgument(args, "targetAgentId");
  const message = stringArgument(args, "message");
  const expectsReply = args.expectsReply !== false;
  const familyContext = await loadOrCreateFamily(context);
  const { family, rootAgent } = familyContext;
  const sender = currentFamilyAgent(family, context);
  const target = resolveFamilyAgent(family, targetRef);
  if (!target) {
    return { ok: false, error: `fork target not found: ${targetRef}` };
  }
  if (target.status === "killed") {
    return { ok: false, error: `fork target is killed: ${targetRef}` };
  }
  const targetAgent = await context.exoharness.getAgent(target.agentId);
  if (!targetAgent) {
    return { ok: false, error: `target agent record not found: ${targetRef}` };
  }
  const conversationRef =
    stringOrNull(args.conversationSlug) ??
    target.conversationId ??
    target.conversationSlug;
  if (!conversationRef) {
    return {
      ok: false,
      error: "target fork does not have a default conversation",
    };
  }
  const conversation = await resolveConversation(targetAgent, conversationRef);
  if (!conversation) {
    return {
      ok: false,
      error: `target conversation not found: ${conversationRef}`,
    };
  }
  const event: FamilyEvent = {
    id: eventId(),
    createdAt: new Date().toISOString(),
    type: "fork_message_sent",
    actorAgentId: sender.agentId,
    fromAgentId: sender.agentId,
    toAgentId: target.agentId,
    toConversationId: conversation.record.id,
    message,
    expectsReply,
  };
  family.events.push(event);
  await saveFamily(rootAgent, family);
  const delivery = deliverForkMessage({
    family,
    target,
    conversationSlug: conversation.record.slug,
    fromAgentId: sender.agentId,
    fromAgentName: sender.name,
    message,
    expectsReply,
    sandboxProvider:
      context.conversationConfig.sandboxProvider ??
      context.agentConfig.sandboxProvider,
  });
  return {
    ok: true,
    familyId: family.familyId,
    fromAgentId: sender.agentId,
    targetAgentId: target.agentId,
    targetConversationId: conversation.record.id,
    eventId: event.id,
    delivery: {
      mode: "detached_turn",
      pid: delivery.pid,
      logPath: delivery.logPath,
    },
  };
}

// getConversation is an id-only lookup on the harness protocol, so slugs must
// be matched by listing.
async function resolveConversation(
  agent: Agent,
  ref: string,
): Promise<Conversation | null> {
  const conversations = await agent.listConversations();
  return (
    conversations.find(
      (conversation) =>
        conversation.record.id === ref || conversation.record.slug === ref,
    ) ?? null
  );
}

// Build the developer message that tells a fork-family member who it is.
// Returns null for agents that are not part of any fork family. This is
// injected every turn (like memoryInstruction) so a child knows it is a child
// in every conversation, not just the one seeded at fork time.
export async function forkInstruction(
  context: TurnContext,
): Promise<Message | null> {
  let family: FamilyStore;
  let self: FamilyAgentRecord;
  try {
    const current = context.exoharness.current.agent;
    const ref = await readLatestJsonArtifact<FamilyRef>(
      current,
      FAMILY_REF_ARTIFACT_PATH,
    );
    if (!ref) {
      return null;
    }
    const rootAgent = await context.exoharness.getAgent(ref.rootAgentId);
    if (!rootAgent) {
      return null;
    }
    const store = await readLatestJsonArtifact<FamilyStore>(
      rootAgent,
      FAMILY_ARTIFACT_PATH,
    );
    if (!store || store.familyId !== ref.familyId) {
      return null;
    }
    const record = store.agents.find(
      (agent) => agent.agentId === current.record.id,
    );
    if (!record) {
      return null;
    }
    family = store;
    self = record;
  } catch (error) {
    // Prompt assembly runs every model round; a broken ledger must not brick
    // the agent. Degrade to no lineage message and log loudly.
    const detail = error instanceof Error ? error.message : String(error);
    console.error(`fork lineage unavailable during prompt assembly: ${detail}`);
    return null;
  }

  if (self.parentAgentId === null) {
    const activeChildren = family.agents.filter(
      (agent) => agent.parentAgentId !== null && agent.status !== "killed",
    );
    if (activeChildren.length === 0) {
      return null;
    }
    const lines = activeChildren.map(
      (child) =>
        `- ${child.slug} (${child.status}${child.purpose ? `, purpose: ${child.purpose}` : ""})`,
    );
    return {
      role: "developer",
      content: `You are the root agent of fork family ${family.familyId} and have active child forks:\n${lines.join("\n")}\nUse list_forks and list_fork_events to inspect them, send_fork_message to coordinate, and kill_fork to stop a child.`,
    };
  }

  const parent = family.agents.find(
    (agent) => agent.agentId === self.parentAgentId,
  );
  const parentLabel = parent
    ? `${parent.name} (${parent.slug}, agent ${parent.agentId})`
    : `agent ${self.parentAgentId}`;
  return {
    role: "developer",
    content: [
      `You are a child fork in fork family ${family.familyId}, generation ${self.generation}, forked from ${parentLabel} on ${self.createdAt}.`,
      self.purpose ? `Your fork purpose: ${self.purpose}.` : null,
      `Your own source worktree is at ${self.sourceRoot} on the host and is mounted as your sandbox source tree; changes there do not affect your parent's checkout.`,
      `Your fork_birth event is recorded in your conversation event log and in the shared family ledger. Use list_forks and list_fork_events to inspect your family, and send_fork_message to report progress or ask your parent (${parent?.slug ?? self.parentAgentId}) for guidance.`,
    ]
      .filter((line) => line !== null)
      .join(" "),
  };
}

async function loadOrCreateFamily(
  context: TurnContext,
): Promise<FamilyContext> {
  const current = context.exoharness.current.agent;
  const existingRef = await readLatestJsonArtifact<FamilyRef>(
    current,
    FAMILY_REF_ARTIFACT_PATH,
  );
  if (existingRef) {
    const rootAgent = await context.exoharness.getAgent(
      existingRef.rootAgentId,
    );
    if (!rootAgent) {
      throw new Error(
        `fork family root agent not found: ${existingRef.rootAgentId}`,
      );
    }
    const family = await readLatestJsonArtifact<FamilyStore>(
      rootAgent,
      FAMILY_ARTIFACT_PATH,
    );
    if (!family || family.familyId !== existingRef.familyId) {
      throw new Error(
        `fork family artifact is missing or corrupt: ${existingRef.familyId}`,
      );
    }
    return { family, rootAgent };
  }

  const now = new Date().toISOString();
  const family: FamilyStore = {
    familyId: `fam_${randomUUID()}`,
    rootAgentId: current.record.id,
    rootAgentSlug: current.record.slug,
    createdAt: now,
    agents: [
      {
        agentId: current.record.id,
        slug: current.record.slug,
        name: current.record.name,
        parentAgentId: null,
        generation: 0,
        status: "active",
        purpose: "root",
        nodePath: "root",
        sourceRoot: ROOT_DIR,
        conversationId: context.exoharness.current.conversation.record.id,
        conversationSlug: context.exoharness.current.conversation.record.slug,
        createdAt: now,
      },
    ],
    events: [],
  };
  await writeFamilyRef(current, {
    familyId: family.familyId,
    rootAgentId: current.record.id,
  });
  await materializeTribeNode(family, family.agents[0]);
  await saveFamily(current, family);
  return { family, rootAgent: current };
}

async function saveFamily(
  rootAgent: Agent,
  family: FamilyStore,
): Promise<void> {
  await rootAgent.writeArtifactJson({
    path: FAMILY_ARTIFACT_PATH,
    value: family as unknown as JsonValue,
  });
  await materializeTribeIndex(family);
}

async function writeFamilyRef(agent: Agent, ref: FamilyRef): Promise<void> {
  await agent.writeArtifactJson({
    path: FAMILY_REF_ARTIFACT_PATH,
    value: ref as unknown as JsonValue,
  });
}

function currentFamilyAgent(
  family: FamilyStore,
  context: TurnContext,
): FamilyAgentRecord {
  const agentId = context.exoharness.current.agent.record.id;
  const record = family.agents.find((agent) => agent.agentId === agentId);
  if (!record) {
    throw new Error(`current agent is not in fork family: ${agentId}`);
  }
  return record;
}

function resolveFamilyAgent(
  family: FamilyStore,
  ref: string,
): FamilyAgentRecord | null {
  return (
    family.agents.find(
      (agent) => agent.agentId === ref || agent.slug === ref,
    ) ?? null
  );
}

function isDescendantOf(
  family: FamilyStore,
  childAgentId: string,
  ancestorAgentId: string,
): boolean {
  let current = family.agents.find((agent) => agent.agentId === childAgentId);
  while (current?.parentAgentId) {
    if (current.parentAgentId === ancestorAgentId) {
      return true;
    }
    current = family.agents.find(
      (agent) => agent.agentId === current?.parentAgentId,
    );
  }
  return false;
}

function nextChildIndex(family: FamilyStore, parentAgentId: string): number {
  return (
    family.agents.filter((agent) => agent.parentAgentId === parentAgentId)
      .length + 1
  );
}

function nodeNameFor(
  requestedSlug: string | null,
  purpose: string | null,
  childIndex: number,
): string {
  const suffix = slugify(requestedSlug ?? purpose ?? "");
  const prefix = `fork-${String(childIndex).padStart(3, "0")}`;
  return suffix ? `${prefix}-${suffix}` : prefix;
}

function uniqueChildSlug(requested: string, agents: Agent[]): string {
  const base = slugify(requested) || "fork";
  const existing = new Set(agents.map((agent) => agent.record.slug));
  if (!existing.has(base)) {
    return base;
  }
  for (let index = 1; ; index += 1) {
    const candidate = `${base}-${index}`;
    if (!existing.has(candidate)) {
      return candidate;
    }
  }
}

function titleize(value: string): string {
  return slugify(value)
    .split("-")
    .filter(Boolean)
    .map((part) => `${part[0]?.toUpperCase() ?? ""}${part.slice(1)}`)
    .join(" ");
}

function slugify(value: string): string {
  return value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 64);
}

async function copyLatestJsonArtifact(
  source: Agent,
  target: Agent,
  artifactPath: string,
): Promise<void> {
  const value = await readLatestJsonArtifact<JsonValue>(source, artifactPath);
  if (value !== null) {
    await target.writeArtifactJson({ path: artifactPath, value });
  }
}

async function readLatestJsonArtifact<T>(
  agent: Agent,
  artifactPath: string,
): Promise<T | null> {
  const latest = latestArtifactVersion(
    await agent.listArtifacts(),
    artifactPath,
  );
  if (!latest) {
    return null;
  }
  return agent.readArtifactJson<T>({
    artifactId: latest.artifactId,
    version: latest.version,
  });
}

function latestArtifactVersion(
  artifacts: ArtifactVersion[],
  artifactPath: string,
): ArtifactVersion | null {
  const versions = artifacts
    .filter((artifact) => artifact.path === artifactPath)
    .sort((left, right) => left.version - right.version);
  return versions.at(-1) ?? null;
}

// Delivers a fork message by starting a one-shot turn on the target
// conversation through the exo CLI. Custom conversation events are not
// rendered into model history and do not trigger turns, so delivery must be a
// real send. The send is detached so the sender's turn does not block on (or
// recurse into) the target's turn; output goes to the target's tribe state
// log. TODO(fork-runtime): move delivery into the Rust host-tool layer with
// send_conversation_wakeup once kill enforcement lands there.
function deliverForkMessage(args: {
  family: FamilyStore;
  target: FamilyAgentRecord;
  conversationSlug: string;
  fromAgentId: string;
  fromAgentName: string;
  message: string;
  expectsReply: boolean;
  sandboxProvider: string;
}): { pid: number | null; logPath: string } {
  const logPath = path.join(
    tribeNodeDir(args.family, args.target),
    "state",
    "fork-messages.log",
  );
  mkdirSync(path.dirname(logPath), { recursive: true });
  const logFd = openSync(logPath, "a");
  const replyNote = args.expectsReply
    ? "The sender expects a reply; respond with send_fork_message."
    : "No reply is expected.";
  const prompt = `Internal fork message from ${args.fromAgentName} (agent ${args.fromAgentId}) in your fork family ${args.family.familyId}. ${replyNote}\n\n${args.message}`;
  // The detached process must run with the same sandbox backend as the
  // current agent, otherwise the target's turn fails when its configured
  // sandbox provider (e.g. docker) is not in the spawned CLI's default
  // backend set. The CLI reads EXO_SANDBOX_BACKEND as the flag fallback.
  const backend = sandboxBackendFor(args.sandboxProvider);
  const child = spawn(
    EXO_BIN,
    [
      "--env-file-if-exists",
      path.join(ROOT_DIR, ".env"),
      "conversation",
      "send",
      args.target.slug,
      args.conversationSlug,
      prompt,
    ],
    {
      cwd: ROOT_DIR,
      detached: true,
      stdio: ["ignore", logFd, logFd],
      env: backend
        ? { ...process.env, EXO_SANDBOX_BACKEND: backend }
        : process.env,
    },
  );
  child.unref();
  closeSync(logFd);
  return { pid: child.pid ?? null, logPath };
}

// Maps an agent config sandbox provider to the exo CLI --sandbox-backend
// value that supports it. Daytona is remote and needs no local backend.
function sandboxBackendFor(provider: string): string | null {
  switch (provider) {
    case "docker":
      return "docker";
    case "apple_container":
      return "apple-container";
    case "local_process":
      return "local-process";
    default:
      return null;
  }
}

async function materializeTribeIndex(family: FamilyStore): Promise<void> {
  const tribeDir = path.join(TRIBE_ROOT, family.rootAgentSlug);
  await mkdir(path.join(tribeDir, "events"), { recursive: true });
  await mkdir(path.join(tribeDir, "agents"), { recursive: true });
  await writeJsonFile(path.join(tribeDir, "tribe.json"), {
    familyId: family.familyId,
    rootAgentId: family.rootAgentId,
    rootAgentSlug: family.rootAgentSlug,
    createdAt: family.createdAt,
  });
  for (const agent of family.agents) {
    await writeJsonFile(
      path.join(tribeDir, "agents", `${agent.agentId}.json`),
      {
        agentId: agent.agentId,
        slug: agent.slug,
        nodePath: agent.nodePath,
        status: agent.status,
      },
    );
  }
  for (const event of family.events) {
    await writeJsonFile(
      path.join(tribeDir, "events", `${event.id}.json`),
      event,
    );
  }
}

function tribeNodeDir(family: FamilyStore, agent: FamilyAgentRecord): string {
  return path.join(TRIBE_ROOT, family.rootAgentSlug, agent.nodePath);
}

async function materializeTribeNode(
  family: FamilyStore,
  agent: FamilyAgentRecord,
): Promise<void> {
  const nodeDir = tribeNodeDir(family, agent);
  await mkdir(path.join(nodeDir, "state"), { recursive: true });
  await mkdir(path.join(nodeDir, "children"), { recursive: true });
  await writeJsonFile(path.join(nodeDir, "agent.json"), agent);
}

// The child repo is a standalone local clone, not a linked git worktree. A
// worktree's .git is a pointer into the parent repo's .git/worktrees/, which
// is not mounted into the child's sandbox, so git would be broken there.
// A local clone hardlinks objects (cheap) and is fully self-contained. The
// parent can integrate child work with `git fetch <sourceRoot> fork/<slug>`.
async function createSourceClone(
  baseSourceRoot: string,
  sourceRoot: string,
  childSlug: string,
): Promise<void> {
  await mkdir(path.dirname(sourceRoot), { recursive: true });
  try {
    await execFileAsync("git", [
      "clone",
      "--local",
      baseSourceRoot,
      sourceRoot,
    ]);
    await execFileAsync("git", [
      "-C",
      sourceRoot,
      "checkout",
      "-b",
      `fork/${childSlug}`,
    ]);
  } catch (error) {
    // A collision here means stale state (for example a repo left behind by
    // an earlier fork). Fail loudly rather than silently returning a child
    // with the wrong or missing repo.
    throw new Error(
      `failed to create child source clone at ${sourceRoot} (branch fork/${childSlug}): ${gitErrorMessage(error)}`,
    );
  }
}

async function writeManageScript(
  family: FamilyStore,
  agent: FamilyAgentRecord,
): Promise<void> {
  const nodeDir = tribeNodeDir(family, agent);
  const script = `#!/usr/bin/env bash
set -euo pipefail

NODE_DIR="$(cd "$(dirname "$0")" && pwd)"
AGENT_JSON="$NODE_DIR/agent.json"
SOURCE_ROOT="$(node -e 'const fs=require("fs"); const j=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); console.log(j.sourceRoot)' "$AGENT_JSON")"
AGENT_SLUG="$(node -e 'const fs=require("fs"); const j=JSON.parse(fs.readFileSync(process.argv[1],"utf8")); console.log(j.slug)' "$AGENT_JSON")"
ROOT_DIR="$(cd "$SOURCE_ROOT" && pwd)"

case "\${1:-status}" in
  status)
    echo "agent: $AGENT_SLUG"
    echo "source: $SOURCE_ROOT"
    ;;
  build)
    (cd "$SOURCE_ROOT" && CARGO_TARGET_DIR=target cargo build -p exo --ignore-rust-version)
    ;;
  stop)
    echo "stop is not fully implemented yet for child agents"
    ;;
  start)
    echo "start is not fully implemented yet for child agents"
    ;;
  restart)
    "$0" stop
    "$0" build
    "$0" start
    ;;
  logs)
    echo "logs are not fully implemented yet for child agents"
    ;;
  *)
    echo "usage: $0 {status|stop|build|start|restart|logs}" >&2
    exit 2
    ;;
esac
`;
  const managePath = path.join(nodeDir, "manage");
  await writeFile(managePath, script, "utf8");
  await chmod(managePath, 0o755);
}

async function writeJsonFile(filePath: string, value: unknown): Promise<void> {
  await mkdir(path.dirname(filePath), { recursive: true });
  await writeFile(filePath, `${JSON.stringify(value, null, 2)}\n`, "utf8");
}

function stringArgument(args: JsonObject, key: string): string {
  const value = args[key];
  if (typeof value !== "string" || value.trim().length === 0) {
    throw new Error(`${key} must be a non-empty string`);
  }
  return value.trim();
}

function stringOrNull(value: JsonValue | undefined): string | null {
  return typeof value === "string" && value.trim().length > 0
    ? value.trim()
    : null;
}

function numberOrNull(value: JsonValue | undefined): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function eventId(): string {
  return `fork_evt_${randomUUID()}`;
}

function rustAgentConfig(context: TurnContext): JsonObject {
  return {
    instructions: context.agentConfig.instructions as unknown as JsonValue,
    harness: context.agentConfig.harness,
    typescript: context.agentConfig.typescript
      ? {
          module_path: context.agentConfig.typescript.modulePath,
          tool_module_paths: context.agentConfig.typescript.toolModulePaths,
        }
      : null,
    enable_agent_tool_creation: context.agentConfig.enableAgentToolCreation,
    sandbox_image: context.agentConfig.sandboxImage ?? null,
    sandbox_provider: context.agentConfig.sandboxProvider,
    enable_networking: context.agentConfig.enableNetworking,
    model: context.agentConfig.model,
    max_output_tokens: context.agentConfig.maxOutputTokens ?? null,
    max_tool_round_trips: context.agentConfig.maxToolRoundTrips ?? null,
    braintrust: (context.agentConfig.braintrust ?? null) as JsonValue,
  };
}

function rustConversationConfig(
  context: TurnContext,
  sourceRewrite: { fromSourceRoot: string; toSourceRoot: string },
): JsonObject {
  return {
    sandbox_image: context.conversationConfig.sandboxImage ?? null,
    sandbox_provider: context.conversationConfig.sandboxProvider ?? null,
    shell_program: context.conversationConfig.shellProgram ?? null,
    // The parent's self-repo mount points at the parent's source checkout.
    // The child must see its own worktree at the same sandbox path, otherwise
    // "editing its own source" would mutate the parent's checkout.
    mounts: context.conversationConfig.mounts.map((mount) => ({
      host_path:
        mount.hostPath === sourceRewrite.fromSourceRoot
          ? sourceRewrite.toSourceRoot
          : mount.hostPath,
      mount_path: mount.mountPath,
      mode: mount.mode,
      internal: mount.internal ?? null,
    })),
    durable_file_systems: [],
    sandbox_scope: context.conversationConfig.sandboxScope ?? null,
  };
}
