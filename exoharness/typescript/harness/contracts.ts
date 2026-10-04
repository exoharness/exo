// Backend-independent contracts for the TypeScript ExoHarness surface.
// Core CRUD, recent-first ordering, turn history, and artifact event assertions
// mirror crates/exoharness/src/contract_tests.rs. Rust-only APIs (thread aliases,
// list pagination, environments, sandbox backends) are outside this interface.
import assert from "node:assert/strict";
import type { Agent, Conversation, ExoHarness, Message, Turn } from "./core";

export interface HarnessContractFixture {
  harness: ExoHarness;
  // Lifecycle operations are provided by the host, outside the TS agent API.
  beginTurn(
    agent: Agent,
    conversation: Conversation,
    input: Message[],
    sessionId?: string,
  ): Promise<ExoHarness>;
  finishTurn(turn: Turn): Promise<string>;
}

async function newThread(harness: ExoHarness) {
  const agent = await harness.newAgent({
    slug: `contract-${crypto.randomUUID()}`,
    name: "Contract agent",
  });
  return { agent, conversation: await agent.newConversation() };
}
const message = (content: string): Message => ({ role: "user", content });
const destinationPolicy = {
  networking: {
    type: "destinations" as const,
    allowedDestinations: [
      { type: "origin" as const, origin: "https://model.example" },
    ],
  },
  injectionLocation: { header: true },
};

export const harnessContracts: Record<
  string,
  (fixture: HarnessContractFixture) => Promise<void>
> = {
  async agent_and_conversation_crud({ harness }) {
    const slug = `contract-${crypto.randomUUID()}`;
    const agent = await harness.newAgent({ slug, name: "Contract agent" });
    const agentId = agent.record.id;
    const conversation = await agent.newConversation({
      slug: "thread",
      name: "Thread",
    });
    const conversationId = conversation.record.id;
    assert.equal((await harness.getAgent(agentId))?.record.slug, slug);
    assert.equal(
      (await agent.getConversation(conversationId))?.record.name,
      "Thread",
    );
    assert(
      (await harness.listAgents()).some((item) => item.record.id === agentId),
    );
    assert(
      (await agent.listConversations()).some(
        (item) => item.record.id === conversationId,
      ),
    );
    assert(
      (await conversation.getEvents()).events.some(
        (event) => event.data.type === "thread_created",
      ),
    );
    await assert.rejects(harness.newAgent({ slug, name: "Duplicate" }));
    await assert.rejects(agent.newConversation({ slug: "thread" }));
    assert.equal(await harness.getAgent(crypto.randomUUID()), null);
    assert.equal(await agent.getConversation(crypto.randomUUID()), null);
    assert.equal(await agent.deleteConversation(conversationId), true);
    assert.equal(await agent.deleteConversation(conversationId), false);
    assert.equal(await agent.getConversation(conversationId), null);
    const reused = await agent.newConversation({ slug: "thread" });
    await agent.deleteConversation(reused.record.id);
    assert.equal(await harness.deleteAgent(agentId), true);
    assert.equal(await harness.deleteAgent(agentId), false);
    const replacement = await harness.newAgent({ slug, name: "Reused slug" });
    await harness.deleteAgent(replacement.record.id);
  },

  async conversations_are_recent_first({ harness }) {
    const agent = await harness.newAgent({
      slug: `contract-${crypto.randomUUID()}`,
      name: "Recent threads",
    });
    const first = await agent.newConversation({ slug: "first" });
    const second = await agent.newConversation({ slug: "second" });
    const third = await agent.newConversation({ slug: "third" });
    await first.addEvents({
      data: [{ type: "custom", event_type: "touch", payload: null }],
    });
    assert.deepEqual(
      (await agent.listConversations()).map((item) => item.record.id),
      [first.record.id, third.record.id, second.record.id],
    );
  },

  async event_cursors_and_filters({ harness }) {
    const { conversation } = await newThread(harness);
    const sessionId = await conversation.startSession();
    const added = await conversation.addEvents({
      sessionId,
      data: [
        {
          type: "custom",
          event_type: "contract.marker",
          payload: { index: 1 },
        },
        { type: "messages", messages: [message("hello")] },
        {
          type: "custom",
          event_type: "contract.marker",
          payload: { index: 2 },
        },
      ],
    });
    const page = await conversation.getEvents({
      types: ["contract.marker"],
      limit: 1,
    });
    assert.equal(page.events.length, 1);
    assert.equal(page.events[0].id, added.eventIds[0]);
    assert.equal(page.cursor, added.eventIds[0]);
    const next = await conversation.getEvents({
      types: ["contract.marker"],
      cursor: page.cursor,
    });
    assert.deepEqual(
      next.events.map((event) => event.id),
      [added.eventIds[2]],
    );
    const backwards = await conversation.getEvents({
      types: ["contract.marker"],
      direction: "desc",
      cursor: added.eventIds[2],
    });
    assert.deepEqual(
      backwards.events.map((event) => event.id),
      [added.eventIds[0]],
    );
    assert.equal((await conversation.getEvents({ limit: 0 })).events.length, 0);
    assert.equal(
      (await conversation.getEvents({ types: ["custom"] })).events.length,
      2,
    );
    assert.equal(
      (await conversation.getEvents({ sessionId: crypto.randomUUID() })).events
        .length,
      0,
    );
    assert.deepEqual((await conversation.getEvent(added.eventIds[1]))?.data, {
      type: "messages",
      messages: [message("hello")],
    });
    assert.equal(await conversation.getEvent(crypto.randomUUID()), null);
    const all = (await conversation.getEvents()).events;
    assert.deepEqual(
      all.map((event) => event.id),
      all.map((event) => event.id).sort(),
    );
    assert.equal(new Set(all.map((event) => event.id)).size, all.length);
    await conversation.endSession(sessionId);
    await assert.rejects(
      conversation.addEvents({
        sessionId,
        data: [{ type: "custom", event_type: "late", payload: null }],
      }),
    );
  },

  async begin_turn_tracks_events_through_finish(fixture) {
    const { agent, conversation } = await newThread(fixture.harness);
    const active = await fixture.beginTurn(agent, conversation, [
      message("ping"),
    ]);
    const turn = active.current.turn;
    assert.equal(active.current.agent.record.id, agent.record.id);
    assert.equal(active.current.conversation.record.id, conversation.record.id);
    await turn.addEvents([
      { type: "messages", messages: [{ role: "assistant", content: "pong" }] },
    ]);
    const latestEventId = await fixture.finishTurn(turn);
    const { events } = await conversation.getEvents({ turnId: turn.turnId });
    assert(
      events.some((event) => event.data.type === "session_started"),
      "automatically created session must belong to its first turn",
    );
    assert(events.some((event) => event.data.type === "turn_started"));
    assert.equal(
      events.filter((event) => event.data.type === "messages").length,
      2,
    );
    assert.equal(events.at(-1)?.data.type, "turn_ended");
    assert.equal(events.at(-1)?.id, latestEventId);
    assert.equal(conversation.record.latestEventId, latestEventId);
    assert(
      events.every(
        (event) =>
          event.sessionId === turn.sessionId && event.turnId === turn.turnId,
      ),
    );
    const again = await fixture.beginTurn(
      agent,
      conversation,
      [],
      turn.sessionId,
    );
    assert.notEqual(again.current.turn.turnId, turn.turnId);
    assert.equal(
      (
        await conversation.getEvents({
          turnId: again.current.turn.turnId,
          types: ["session_started"],
        })
      ).events.length,
      0,
    );
    await fixture.finishTurn(again.current.turn);
    await conversation.endSession(turn.sessionId);
  },

  async turn_artifacts_preserve_event_ownership(fixture) {
    const { agent, conversation } = await newThread(fixture.harness);
    await conversation.writeArtifactText({
      path: "outside.txt",
      text: "outside turn",
    });
    const active = await fixture.beginTurn(agent, conversation, [
      message("ping"),
    ]);
    const turn = active.current.turn;
    const artifact = await turn.writeArtifactJson({
      path: "tool-results/example.json",
      value: { ok: true },
    });
    await turn.addEvents([
      { type: "messages", messages: [{ role: "assistant", content: "pong" }] },
    ]);
    await fixture.finishTurn(turn);
    const { events } = await conversation.getEvents({
      types: ["artifact_written"],
    });
    assert.equal(
      events.length,
      2,
      "conversation and turn artifact writes must emit canonical events",
    );
    assert.equal(events[0].sessionId ?? null, null);
    assert.equal(events[0].turnId ?? null, null);
    assert.equal(events[1].sessionId, turn.sessionId);
    assert.equal(events[1].turnId, turn.turnId);
    assert.deepEqual(events[1].data, {
      type: "artifact_written",
      artifact_id: artifact.artifactId,
      path: artifact.path,
      version: artifact.version,
    });
    assert.deepEqual(
      await conversation.readArtifactJson({ artifactId: artifact.artifactId }),
      { ok: true },
    );
  },

  async artifact_versions_and_scope_isolation({ harness }) {
    const { agent, conversation } = await newThread(harness);
    const conversationId = conversation.record.id;
    const first = await conversation.writeArtifact({
      path: "bytes.bin",
      contents: new Uint8Array([0, 255, 1]),
    });
    const second = await conversation.writeArtifactText({
      path: "bytes.bin",
      text: "new contents",
    });
    assert.equal(second.artifactId, first.artifactId);
    assert.equal(second.version, first.version + 1);
    assert.deepEqual(
      [
        ...((
          await conversation.readArtifact({
            artifactId: first.artifactId,
            version: first.version,
          })
        )?.contents ?? []),
      ],
      [0, 255, 1],
    );
    assert.equal(
      await conversation.readArtifactText({ artifactId: first.artifactId }),
      "new contents",
    );
    assert.deepEqual(
      (await conversation.listArtifacts()).map((item) => item.version),
      [second.version],
    );
    const sibling = await agent.newConversation();
    assert.equal(
      await sibling.readArtifact({ artifactId: first.artifactId }),
      null,
    );
    assert.equal(
      await agent.readArtifact({ artifactId: first.artifactId }),
      null,
    );
    const agentArtifact = await agent.writeArtifactText({
      path: "bytes.bin",
      text: "agent scope",
    });
    assert.notEqual(agentArtifact.artifactId, first.artifactId);
    assert.equal(
      await conversation.readArtifact({ artifactId: agentArtifact.artifactId }),
      null,
    );
    await agent.deleteConversation(conversationId);
    assert.equal(await agent.getConversation(conversationId), null);
    assert.equal(
      await agent.readArtifactText({ artifactId: agentArtifact.artifactId }),
      "agent scope",
    );
  },

  async forks_copy_history_through_cursor_and_isolate_artifacts({ harness }) {
    const { conversation } = await newThread(harness);
    await conversation.writeArtifactText({
      path: "file.txt",
      text: "original",
    });
    const included = await conversation.addEvents({
      data: [{ type: "custom", event_type: "included", payload: null }],
    });
    await conversation.addEvents({
      data: [{ type: "custom", event_type: "excluded", payload: null }],
    });
    const fork = await conversation.fork({
      upToInclusive: included.latestEventId,
    });
    const { events } = await fork.getEvents();
    assert.equal(
      (await fork.getEvents({ types: ["included"] })).events.length,
      1,
    );
    assert.equal(
      (await fork.getEvents({ types: ["excluded"] })).events.length,
      0,
    );
    assert(
      events.some(
        (event) =>
          event.data.type === "thread_forked" &&
          event.data.source_thread_id === conversation.record.id,
      ),
    );
    const originalIds = new Set(
      (await conversation.getEvents()).events.map((event) => event.id),
    );
    assert(
      events.every(
        (event) =>
          event.conversationId === fork.record.id && !originalIds.has(event.id),
      ),
    );
    const copy = (await fork.listArtifacts()).find(
      (item) => item.path === "file.txt",
    );
    assert(copy);
    assert.equal(
      await fork.readArtifactText({ artifactId: copy.artifactId }),
      "original",
    );
    await fork.writeArtifactText({ path: "file.txt", text: "fork edit" });
    const original = (await conversation.listArtifacts()).find(
      (item) => item.path === "file.txt",
    )!;
    assert.equal(
      await conversation.readArtifactText({ artifactId: original.artifactId }),
      "original",
    );
    await assert.rejects(
      conversation.fork({ upToInclusive: crypto.randomUUID() }),
    );
  },

  async vault_scopes_revisions_and_destination_policy({ harness }) {
    const global = await harness.createVault("global");
    const attached = await harness.createVault("attached");
    const privateVault = await harness.createVault("private");
    const agent = await harness.newAgent({
      slug: `contract-${crypto.randomUUID()}`,
      name: "Vault scope",
      vaults: [attached.record.id],
    });
    const conversation = await agent.newConversation({
      vaults: [privateVault.record.id],
    });
    const sibling = await agent.newConversation();
    assert.deepEqual(
      new Set((await agent.listVaults()).map((v) => v.record.id)),
      new Set([global.record.id, attached.record.id]),
    );
    assert.deepEqual(
      new Set((await conversation.listVaults()).map((v) => v.record.id)),
      new Set([global.record.id, attached.record.id, privateVault.record.id]),
    );
    assert.equal(await sibling.getVault(privateVault.record.id), null);
    const id = await attached.putSecret({
      name: "KEY",
      secret: { type: "key", value: "synthetic-one" },
      policy: destinationPolicy,
    });
    assert.deepEqual(
      await attached.resolveSecret(id, {
        type: "url",
        url: "https://model.example/v1/responses",
      }),
      { revision: 1, secret: { type: "key", value: "synthetic-one" } },
    );
    await assert.rejects(
      attached.resolveSecret(id, {
        type: "origin",
        origin: "https://other.example",
      }),
    );
    await assert.rejects(
      attached.resolveSecret(id, {
        type: "url",
        url: "http://model.example/v1/responses",
      }),
    );
    const updated = await attached.updateSecret(id, {
      secret: { type: "key", value: "synthetic-two" },
    });
    assert.equal(updated.revision, 2);
    assert.deepEqual(await attached.getSecret(id), {
      type: "key",
      value: "synthetic-two",
    });
    await attached.deleteSecret(id);
    assert.equal(await attached.getSecret(id), null);
    await assert.rejects(
      attached.resolveSecret(id, {
        type: "origin",
        origin: "https://model.example",
      }),
    );
    const privateVaultId = privateVault.record.id;
    await harness.deleteVault(privateVaultId);
    assert.equal(await conversation.getVault(privateVaultId), null);
  },
};

export interface HarnessCheckpoint {
  agentId: string;
  conversationId: string;
  artifactId: string;
  vaultId: string;
  secretId: string;
  lastEventId: string;
}
export async function seedHarnessCheckpoint(
  fixture: HarnessContractFixture,
): Promise<HarnessCheckpoint> {
  const vault = await fixture.harness.createVault("persisted");
  const agent = await fixture.harness.newAgent({
    slug: "persisted",
    name: "Persistence",
    vaults: [vault.record.id],
  });
  const conversation = await agent.newConversation();
  const active = await fixture.beginTurn(agent, conversation, [
    message("before restart"),
  ]);
  const artifact = await active.current.turn.writeArtifactText({
    path: "persisted.txt",
    text: "durable contents",
  });
  const lastEventId = await fixture.finishTurn(active.current.turn);
  const secretId = await vault.putSecret({
    name: "KEY",
    secret: { type: "key", value: "synthetic-persisted" },
    policy: destinationPolicy,
  });
  return {
    agentId: agent.record.id,
    conversationId: conversation.record.id,
    artifactId: artifact.artifactId,
    vaultId: vault.record.id,
    secretId,
    lastEventId,
  };
}
export async function verifyHarnessCheckpoint(
  { harness }: HarnessContractFixture,
  saved: HarnessCheckpoint,
): Promise<void> {
  const agent = await harness.getAgent(saved.agentId);
  assert(agent);
  const conversation = await agent.getConversation(saved.conversationId);
  assert(conversation);
  assert.equal(
    await conversation.readArtifactText({ artifactId: saved.artifactId }),
    "durable contents",
  );
  assert.equal(
    (await conversation.getEvents()).events.at(-1)?.id,
    saved.lastEventId,
  );
  const vault = await conversation.getVault(saved.vaultId);
  assert(vault);
  assert.deepEqual(
    await vault.resolveSecret(saved.secretId, {
      type: "origin",
      origin: "https://model.example",
    }),
    { revision: 1, secret: { type: "key", value: "synthetic-persisted" } },
  );
  const added = await conversation.addEvents({
    data: [{ type: "custom", event_type: "after_restart", payload: null }],
  });
  assert(
    added.latestEventId > saved.lastEventId,
    "event cursors must remain monotonic after restart",
  );
}
