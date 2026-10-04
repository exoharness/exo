import type {
  AddEventsRequest,
  AddEventsResult,
  Agent,
  AgentRecord,
  Artifact,
  ArtifactVersion,
  Conversation,
  ConversationRecord,
  CredentialDestination,
  CredentialPolicy,
  Event,
  EventData,
  EventQuery,
  ExoHarness,
  ExoHarnessCurrent,
  ForkConversationRequest,
  JsonValue,
  NewConversationRequest,
  ResolvedSecret,
  Secret,
  SecretMetadata,
  Turn,
  TurnRecord,
  Vault,
  VaultRecord,
} from "../../typescript/harness/core";

export class StateStore {
  readonly sql: SqlStorage;

  constructor(sql: SqlStorage) {
    this.sql = sql;
    sql.exec(
      "CREATE TABLE IF NOT EXISTS state (key TEXT PRIMARY KEY, json TEXT NOT NULL)",
    );
  }

  get<T>(key: string): T | null {
    const row = this.sql
      .exec<{ json: string }>("SELECT json FROM state WHERE key = ?", key)
      .toArray()[0];
    return row ? (JSON.parse(row.json) as T) : null;
  }

  put(key: string, value: unknown): void {
    this.sql.exec(
      "INSERT INTO state VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET json = excluded.json",
      key,
      JSON.stringify(value),
    );
  }

  delete(key: string): void {
    this.sql.exec("DELETE FROM state WHERE key = ?", key);
  }

  list<T>(prefix: string): T[] {
    return this.sql
      .exec<{ json: string }>(
        "SELECT json FROM state WHERE substr(key, 1, ?) = ? ORDER BY key",
        prefix.length,
        prefix,
      )
      .toArray()
      .map((row) => JSON.parse(row.json) as T);
  }

  deletePrefix(prefix: string): void {
    this.sql.exec(
      "DELETE FROM state WHERE substr(key, 1, ?) = ?",
      prefix.length,
      prefix,
    );
  }

  // Persist the generator so cursor order survives both same-millisecond writes
  // and Durable Object eviction. UUIDs retain the UUIDv7 version and variant bits.
  id(): string {
    const previous = this.get<{ ms: number; random: string }>("uuid");
    let ms = Date.now();
    let random: bigint;
    if (previous && ms <= previous.ms) {
      ms = previous.ms;
      random = BigInt(previous.random) + 1n;
      if (random >= 1n << 74n) {
        ms += 1;
        random = 0n;
      }
    } else {
      const bytes = crypto.getRandomValues(new Uint8Array(10));
      random =
        BigInt(
          `0x${Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("")}`,
        ) &
        ((1n << 74n) - 1n);
    }
    this.put("uuid", { ms, random: random.toString() });
    const time = ms.toString(16).padStart(12, "0");
    const high = (random >> 62n).toString(16).padStart(3, "0");
    const low = ((random & ((1n << 62n) - 1n)) | (2n << 62n))
      .toString(16)
      .padStart(16, "0");
    return `${time.slice(0, 8)}-${time.slice(8)}-7${high}-${low.slice(0, 4)}-${low.slice(4)}`;
  }
}

function required<T>(value: T | null, message: string): T {
  if (value === null) throw new Error(message);
  return value;
}

function validateName(name: string): void {
  if (!name || name.includes("/") || name.includes("\0"))
    throw new Error("invalid resource name");
}

export class CloudflareExoHarness implements ExoHarness {
  readonly store: StateStore;
  readonly bucket: R2Bucket;
  readonly vaultKey: string;
  private readonly activeContext?: ExoHarnessCurrent & { turn: CloudflareTurn };

  constructor(
    store: StateStore,
    bucket: R2Bucket,
    vaultKey: string,
    current?: ExoHarnessCurrent & { turn: CloudflareTurn },
  ) {
    this.store = store;
    this.bucket = bucket;
    this.vaultKey = vaultKey;
    this.activeContext = current;
  }

  get current(): ExoHarnessCurrent & { turn: CloudflareTurn } {
    return required(this.activeContext ?? null, "no active turn");
  }

  async listAgents(): Promise<Agent[]> {
    return this.store
      .list<AgentRecord>("agent/")
      .map((record) => new CloudflareAgent(this, record.id));
  }

  async getAgent(id: string): Promise<Agent | null> {
    return this.store.get<AgentRecord>(`agent/${id}`)
      ? new CloudflareAgent(this, id)
      : null;
  }

  async newAgent(request: {
    slug: string;
    name: string;
    vaults?: string[];
  }): Promise<Agent> {
    validateName(request.slug);
    if (
      this.store
        .list<AgentRecord>("agent/")
        .some((agent) => agent.slug === request.slug)
    )
      throw new Error("agent slug already exists");
    this.checkVaults(request.vaults ?? []);
    const id = this.store.id();
    this.store.put(`agent/${id}`, {
      id,
      slug: request.slug,
      name: request.name,
      vaults: request.vaults ?? [],
    } satisfies AgentRecord);
    return new CloudflareAgent(this, id);
  }

  async deleteAgent(id: string): Promise<boolean> {
    const agent = await this.getAgent(id);
    if (!agent) return false;
    for (const thread of await agent.listConversations())
      await agent.deleteConversation(thread.record.id);
    await new ArtifactStore(this, `agent:${id}`).deleteAll();
    this.store.delete(`agent/${id}`);
    return true;
  }

  async createVault(name: string): Promise<Vault> {
    validateName(name);
    if (this.store.list<VaultRecord>("vault/").some((v) => v.name === name))
      throw new Error("vault name already exists");
    const id = this.store.id();
    this.store.put(`vault/${id}`, {
      id,
      name,
      createdAt: new Date().toISOString(),
    } satisfies VaultRecord);
    return new CloudflareVault(this, id);
  }

  async deleteVault(id: string): Promise<void> {
    this.store.deletePrefix(`secret/${id}/`);
    this.store.delete(`vault/${id}`);
  }

  async listVaults(): Promise<Vault[]> {
    return this.store
      .list<VaultRecord>("vault/")
      .map((record) => new CloudflareVault(this, record.id));
  }

  async getVault(id: string): Promise<Vault | null> {
    return this.store.get<VaultRecord>(`vault/${id}`)
      ? new CloudflareVault(this, id)
      : null;
  }

  checkVaults(ids: string[]): void {
    for (const id of ids)
      required(this.store.get<VaultRecord>(`vault/${id}`), "vault not found");
  }

  async beginTurn(
    agentId: string,
    threadId: string,
    input: EventData[],
    sessionId?: string,
  ): Promise<Turn> {
    return this.beginTurnRecord(agentId, threadId, input, sessionId);
  }
  beginTurnRecord(
    agentId: string,
    threadId: string,
    input: EventData[],
    sessionId?: string,
  ): CloudflareTurn {
    required(
      this.store.get<AgentRecord>(`agent/${agentId}`),
      "agent not found",
    );
    required(
      this.store.get<ConversationRecord>(`thread/${agentId}/${threadId}`),
      "thread not found",
    );
    const conversation = new CloudflareConversation(this, agentId, threadId);
    if (this.store.get(`active/${threadId}`))
      throw new Error("thread already has an unfinished turn");
    const session = sessionId ?? conversation.startSessionRecord();
    if (!this.store.get(`session/${threadId}/${session}`))
      throw new Error("session not found or ended");
    const record: TurnRecord = { id: this.store.id(), sessionId: session };
    this.store.put(`turn/${threadId}/${record.id}`, record);
    this.store.put(`active/${threadId}`, record);
    conversation.appendEvents({
      sessionId: session,
      turnId: record.id,
      data: [{ type: "turn_started" }, ...input],
    });
    return new CloudflareTurn(this, agentId, threadId, record);
  }

  async forTurn(
    agentId: string,
    threadId: string,
    record: TurnRecord,
  ): Promise<CloudflareExoHarness> {
    const agent = required(await this.getAgent(agentId), "agent not found");
    const conversation = required(
      await agent.getConversation(threadId),
      "thread not found",
    );
    const saved = required(
      this.store.get<TurnRecord>(`turn/${threadId}/${record.id}`),
      "turn not found",
    );
    if (saved.sessionId !== record.sessionId)
      throw new Error("turn session mismatch");
    return new CloudflareExoHarness(this.store, this.bucket, this.vaultKey, {
      agent,
      conversation,
      turn: new CloudflareTurn(this, agentId, threadId, saved),
    });
  }
}

class ArtifactStore {
  readonly harness: CloudflareExoHarness;
  readonly scope: string;
  constructor(harness: CloudflareExoHarness, scope: string) {
    this.harness = harness;
    this.scope = scope;
  }
  get prefix(): string {
    return `artifact/${this.scope}/`;
  }
  objectKey(id: string, version: number): string {
    return `${this.scope}/${id}/${version}`;
  }

  async listArtifacts(): Promise<ArtifactVersion[]> {
    return this.harness.store.list<ArtifactVersion>(`${this.prefix}latest/`);
  }
  async readArtifact(args: {
    artifactId: string;
    version?: number;
  }): Promise<Artifact | null> {
    const meta = this.harness.store.get<ArtifactVersion>(
      args.version === undefined
        ? `${this.prefix}latest/${args.artifactId}`
        : `${this.prefix}version/${args.artifactId}/${args.version}`,
    );
    if (!meta) return null;
    const body = required(
      await this.harness.bucket.get(
        this.objectKey(meta.artifactId, meta.version),
      ),
      "artifact contents missing",
    );
    return { ...meta, contents: new Uint8Array(await body.arrayBuffer()) };
  }
  async readArtifactText(args: {
    artifactId: string;
    version?: number;
  }): Promise<string | null> {
    const artifact = await this.readArtifact(args);
    return artifact ? new TextDecoder().decode(artifact.contents) : null;
  }
  async readArtifactJson<T>(args: {
    artifactId: string;
    version?: number;
  }): Promise<T | null> {
    const text = await this.readArtifactText(args);
    return text === null ? null : (JSON.parse(text) as T);
  }
  async writeArtifact(args: {
    path: string;
    contents: Uint8Array | string;
  }): Promise<ArtifactVersion> {
    if (!args.path || args.path.includes("\0"))
      throw new Error("invalid artifact path");
    const store = this.harness.store;
    const id =
      store.get<string>(`${this.prefix}path/${args.path}`) ?? store.id();
    store.put(`${this.prefix}path/${args.path}`, id);
    const version = (store.get<number>(`${this.prefix}counter/${id}`) ?? 0) + 1;
    store.put(`${this.prefix}counter/${id}`, version);
    const contents =
      typeof args.contents === "string"
        ? new TextEncoder().encode(args.contents)
        : args.contents;
    const meta: ArtifactVersion = {
      artifactId: id,
      path: args.path,
      version,
      sizeBytes: contents.byteLength,
      createdAt: new Date().toISOString(),
    };
    await this.harness.bucket.put(this.objectKey(id, version), contents);
    store.put(`${this.prefix}version/${id}/${version}`, meta);
    const latest = store.get<ArtifactVersion>(`${this.prefix}latest/${id}`);
    if (!latest || latest.version < version)
      store.put(`${this.prefix}latest/${id}`, meta);
    return meta;
  }
  writeArtifactText(args: {
    path: string;
    text: string;
  }): Promise<ArtifactVersion> {
    return this.writeArtifact({ path: args.path, contents: args.text });
  }
  writeArtifactJson(args: {
    path: string;
    value: JsonValue;
  }): Promise<ArtifactVersion> {
    return this.writeArtifactText({
      path: args.path,
      text: JSON.stringify(args.value),
    });
  }
  async deleteAll(): Promise<void> {
    const versions = this.harness.store.list<ArtifactVersion>(
      `${this.prefix}version/`,
    );
    for (const version of versions)
      await this.harness.bucket.delete(
        this.objectKey(version.artifactId, version.version),
      );
    this.harness.store.deletePrefix(this.prefix);
  }
}

class CloudflareAgent extends ArtifactStore implements Agent {
  readonly id: string;
  constructor(harness: CloudflareExoHarness, id: string) {
    super(harness, `agent:${id}`);
    this.id = id;
  }
  get record(): AgentRecord {
    return required(
      this.harness.store.get<AgentRecord>(`agent/${this.id}`),
      "agent not found",
    );
  }
  async listConversations(): Promise<Conversation[]> {
    return this.harness.store
      .list<ConversationRecord>(`thread/${this.id}/`)
      .map(
        (record) =>
          new CloudflareConversation(this.harness, this.id, record.id),
      );
  }
  async getConversation(id: string): Promise<Conversation | null> {
    return this.harness.store.get(`thread/${this.id}/${id}`)
      ? new CloudflareConversation(this.harness, this.id, id)
      : null;
  }
  async newConversation(
    request: NewConversationRequest = {},
  ): Promise<Conversation> {
    const id = this.harness.store.id();
    const slug = request.slug ?? id;
    validateName(slug);
    if (
      this.harness.store
        .list<ConversationRecord>(`thread/${this.id}/`)
        .some((thread) => thread.slug === slug)
    )
      throw new Error("thread slug already exists");
    this.harness.checkVaults(request.vaults ?? []);
    const record: ConversationRecord = {
      id,
      slug,
      name: request.name ?? slug,
      vaults: request.vaults ?? [],
    };
    this.harness.store.put(`thread/${this.id}/${id}`, record);
    const thread = new CloudflareConversation(this.harness, this.id, id);
    await thread.addEvents({
      data: [{ type: "thread_created", slug, name: record.name }],
    });
    return thread;
  }
  async deleteConversation(id: string): Promise<boolean> {
    if (!(await this.getConversation(id))) return false;
    if (this.harness.store.get(`active/${id}`))
      throw new Error("cancel the active turn before deleting its thread");
    await new ArtifactStore(this.harness, `thread:${id}`).deleteAll();
    for (const kind of ["event", "session", "turn"])
      this.harness.store.deletePrefix(`${kind}/${id}/`);
    this.harness.store.delete(`thread/${this.id}/${id}`);
    return true;
  }
  async listVaults(): Promise<Vault[]> {
    return scopedVaults(this.harness, this.record.vaults);
  }
  async getVault(id: string): Promise<Vault | null> {
    return (await this.listVaults()).find((v) => v.record.id === id) ?? null;
  }
}

class CloudflareConversation extends ArtifactStore implements Conversation {
  readonly agentId: string;
  readonly id: string;
  constructor(harness: CloudflareExoHarness, agentId: string, id: string) {
    super(harness, `thread:${id}`);
    this.agentId = agentId;
    this.id = id;
  }
  get record(): ConversationRecord {
    return required(
      this.harness.store.get<ConversationRecord>(
        `thread/${this.agentId}/${this.id}`,
      ),
      "thread not found",
    );
  }
  async listVaults(): Promise<Vault[]> {
    const agent = required(
      await this.harness.getAgent(this.agentId),
      "agent not found",
    );
    return scopedVaults(this.harness, [
      ...agent.record.vaults,
      ...this.record.vaults,
    ]);
  }
  async getVault(id: string): Promise<Vault | null> {
    return (await this.listVaults()).find((v) => v.record.id === id) ?? null;
  }
  async startSession(): Promise<string> {
    return this.startSessionRecord();
  }
  startSessionRecord(): string {
    const id = this.harness.store.id();
    this.harness.store.put(`session/${this.id}/${id}`, true);
    this.appendEvents({
      sessionId: id,
      data: [{ type: "session_started" }],
    });
    return id;
  }
  async endSession(id: string): Promise<void> {
    if (!this.harness.store.get(`session/${this.id}/${id}`))
      throw new Error("session not found or ended");
    if (
      this.harness.store.get<TurnRecord>(`active/${this.id}`)?.sessionId === id
    )
      throw new Error("session has an unfinished turn");
    await this.addEvents({ sessionId: id, data: [{ type: "session_ended" }] });
    this.harness.store.delete(`session/${this.id}/${id}`);
  }
  async getEvent(id: string): Promise<Event | null> {
    return this.harness.store.get<Event>(`event/${this.id}/${id}`);
  }
  async getEvents(
    query: EventQuery = {},
  ): Promise<{ events: Event[]; cursor: string | null }> {
    const direction = query.direction ?? "asc";
    let events = this.harness.store
      .list<Event>(`event/${this.id}/`)
      .filter(
        (event) =>
          (!query.cursor ||
            (direction === "asc"
              ? event.id > query.cursor
              : event.id < query.cursor)) &&
          (!query.sessionId || event.sessionId === query.sessionId) &&
          (!query.turnId || event.turnId === query.turnId) &&
          (!query.types ||
            query.types.includes(event.data.type) ||
            (event.data.type === "custom" &&
              typeof event.data.event_type === "string" &&
              query.types.includes(event.data.event_type))),
      );
    if (direction === "desc") events.reverse();
    if (query.limit != null) {
      if (!Number.isSafeInteger(query.limit) || query.limit < 0)
        throw new Error("invalid event limit");
      events = events.slice(0, query.limit);
    }
    return { events, cursor: events.at(-1)?.id ?? null };
  }
  async addEvents(request: AddEventsRequest): Promise<AddEventsResult> {
    return this.appendEvents(request);
  }
  appendEvents(request: AddEventsRequest): AddEventsResult {
    if (!request.data.length) throw new Error("event batch must not be empty");
    if (
      request.sessionId &&
      !this.harness.store.get(`session/${this.id}/${request.sessionId}`)
    )
      throw new Error("session not found or ended");
    if (request.turnId) {
      const turn = required(
        this.harness.store.get<TurnRecord>(`turn/${this.id}/${request.turnId}`),
        "turn not found",
      );
      if (turn.sessionId !== request.sessionId)
        throw new Error("turn session mismatch");
    }
    const record = this.record;
    const ids = request.data.map((data) => {
      const event: Event = {
        id: this.harness.store.id(),
        conversationId: this.id,
        sessionId: request.sessionId ?? null,
        turnId: request.turnId ?? null,
        createdAt: new Date().toISOString(),
        data,
      };
      this.harness.store.put(`event/${this.id}/${event.id}`, event);
      return event.id;
    });
    const latestEventId = ids[ids.length - 1];
    this.harness.store.put(`thread/${this.agentId}/${this.id}`, {
      ...record,
      latestEventId,
    });
    return { eventIds: ids, latestEventId };
  }
  async fork(request: ForkConversationRequest = {}): Promise<Conversation> {
    if (request.upToInclusive && !(await this.getEvent(request.upToInclusive)))
      throw new Error("fork cursor not found");
    const agent = required(
      await this.harness.getAgent(this.agentId),
      "agent not found",
    );
    const target = await agent.newConversation({
      slug: request.slug,
      name: request.name,
      vaults: this.record.vaults,
    });
    const source = (await this.getEvents()).events.filter(
      (event) => !request.upToInclusive || event.id <= request.upToInclusive,
    );
    // A fork copies canonical history with fresh IDs, not live turn/session ownership.
    if (source.length)
      await target.addEvents({ data: source.map((event) => event.data) });
    for (const meta of await this.listArtifacts()) {
      const artifact = required(
        await this.readArtifact({ artifactId: meta.artifactId }),
        "artifact missing",
      );
      await target.writeArtifact({
        path: meta.path,
        contents: artifact.contents,
      });
    }
    await target.addEvents({
      data: [
        {
          type: "thread_forked",
          source_thread_id: this.id,
          up_to_inclusive: request.upToInclusive ?? null,
        },
      ],
    });
    return target;
  }
}

export class CloudflareTurn extends ArtifactStore implements Turn {
  readonly agentId: string;
  readonly conversationId: string;
  readonly record: TurnRecord;
  constructor(
    harness: CloudflareExoHarness,
    agentId: string,
    threadId: string,
    record: TurnRecord,
  ) {
    super(harness, `thread:${threadId}`);
    this.agentId = agentId;
    this.conversationId = threadId;
    this.record = record;
  }
  get sessionId(): string {
    return this.record.sessionId;
  }
  get turnId(): string {
    return this.record.id;
  }
  get conversation(): Conversation {
    return new CloudflareConversation(
      this.harness,
      this.agentId,
      this.conversationId,
    );
  }
  addEvents(data: EventData[]): Promise<AddEventsResult> {
    return Promise.resolve(this.appendEvents(data));
  }
  appendEvents(data: EventData[]): AddEventsResult {
    return new CloudflareConversation(
      this.harness,
      this.agentId,
      this.conversationId,
    ).appendEvents({
      sessionId: this.sessionId,
      turnId: this.turnId,
      data,
    });
  }
  async finish(): Promise<string> {
    return this.finishRecord();
  }
  finishRecord(): string {
    const active = this.harness.store.get<TurnRecord>(
      `active/${this.conversationId}`,
    );
    if (active?.id !== this.turnId) throw new Error("turn is not active");
    const result = this.appendEvents([{ type: "turn_ended" }]);
    this.harness.store.delete(`active/${this.conversationId}`);
    return result.latestEventId;
  }
}

async function scopedVaults(
  harness: CloudflareExoHarness,
  ids: string[],
): Promise<Vault[]> {
  const global = (await harness.listVaults()).find(
    (v) => v.record.name === "global",
  );
  const scoped = new Set([...(global ? [global.record.id] : []), ...ids]);
  const vaults: Vault[] = [];
  for (const id of scoped) {
    const vault = await harness.getVault(id);
    if (vault) vaults.push(vault);
  }
  return vaults;
}

interface EncryptedSecret {
  metadata: SecretMetadata;
  iv: number[];
  ciphertext: number[];
}

class CloudflareVault implements Vault {
  readonly harness: CloudflareExoHarness;
  readonly id: string;
  constructor(harness: CloudflareExoHarness, id: string) {
    this.harness = harness;
    this.id = id;
  }
  get record(): VaultRecord {
    return required(
      this.harness.store.get<VaultRecord>(`vault/${this.id}`),
      "vault not found",
    );
  }
  async listSecrets(): Promise<SecretMetadata[]> {
    return this.harness.store
      .list<EncryptedSecret>(`secret/${this.id}/`)
      .map((secret) => secret.metadata);
  }
  async putSecret(request: {
    name: string;
    secret: Secret;
    policy?: CredentialPolicy;
  }): Promise<string> {
    required(
      this.harness.store.get<VaultRecord>(`vault/${this.id}`),
      "vault not found",
    );
    validateName(request.name);
    if (request.policy) validateCredentialPolicy(request.policy);
    if (
      this.harness.store
        .list<EncryptedSecret>(`secret/${this.id}/`)
        .some((secret) => secret.metadata.name === request.name)
    )
      throw new Error("secret name already exists");
    const id = this.harness.store.id();
    const metadata: SecretMetadata = {
      id,
      name: request.name,
      type: request.secret.type,
      revision: 1,
      policy: request.policy,
      createdAt: new Date().toISOString(),
    };
    const encrypted = await this.encrypt(metadata, request.secret);
    required(
      this.harness.store.get<VaultRecord>(`vault/${this.id}`),
      "vault not found",
    );
    if (
      this.harness.store
        .list<EncryptedSecret>(`secret/${this.id}/`)
        .some((secret) => secret.metadata.name === request.name)
    )
      throw new Error("secret name already exists");
    this.harness.store.put(`secret/${this.id}/${id}`, encrypted);
    return id;
  }
  async getSecret(id: string): Promise<Secret | null> {
    const encrypted = this.harness.store.get<EncryptedSecret>(
      `secret/${this.id}/${id}`,
    );
    return encrypted ? this.decrypt(encrypted) : null;
  }
  async resolveSecret(
    id: string,
    target: CredentialDestination,
  ): Promise<ResolvedSecret> {
    const encrypted = required(
      this.harness.store.get<EncryptedSecret>(`secret/${this.id}/${id}`),
      "secret not found",
    );
    if (
      !encrypted.metadata.policy ||
      !credentialPermitted(encrypted.metadata.policy, target)
    )
      throw new Error("credential destination denied");
    const secret = await this.decrypt(encrypted);
    if (
      this.harness.store.get<EncryptedSecret>(`secret/${this.id}/${id}`)
        ?.metadata.revision !== encrypted.metadata.revision
    )
      throw new Error("credential changed concurrently");
    return { revision: encrypted.metadata.revision, secret };
  }
  async updateSecret(
    id: string,
    request: { secret?: Secret; policy?: CredentialPolicy },
  ): Promise<SecretMetadata> {
    if (request.policy) validateCredentialPolicy(request.policy);
    const saved = required(
      this.harness.store.get<EncryptedSecret>(`secret/${this.id}/${id}`),
      "secret not found",
    );
    const secret = request.secret ?? (await this.decrypt(saved));
    const metadata: SecretMetadata = {
      ...saved.metadata,
      type: secret.type,
      revision: saved.metadata.revision + 1,
      policy: request.policy ?? saved.metadata.policy,
    };
    const encrypted = await this.encrypt(metadata, secret);
    if (
      this.harness.store.get<EncryptedSecret>(`secret/${this.id}/${id}`)
        ?.metadata.revision !== saved.metadata.revision
    )
      throw new Error("secret changed concurrently; retry the update");
    this.harness.store.put(`secret/${this.id}/${id}`, encrypted);
    return metadata;
  }
  async deleteSecret(id: string): Promise<void> {
    this.harness.store.delete(`secret/${this.id}/${id}`);
  }
  private async key(): Promise<CryptoKey> {
    if (!/^[0-9a-f]{64}$/i.test(this.harness.vaultKey))
      throw new Error("VAULT_KEY must be a 32-byte hex encryption key");
    const bytes = Uint8Array.from(this.harness.vaultKey.match(/../g)!, (part) =>
      Number.parseInt(part, 16),
    );
    return crypto.subtle.importKey("raw", bytes, "AES-GCM", false, [
      "encrypt",
      "decrypt",
    ]);
  }
  private aad(metadata: SecretMetadata): Uint8Array {
    return new TextEncoder().encode(
      JSON.stringify({ vaultId: this.id, metadata }),
    );
  }
  private async encrypt(
    metadata: SecretMetadata,
    secret: Secret,
  ): Promise<EncryptedSecret> {
    if (secret.type !== "key")
      throw new Error("this prototype supports static key credentials only");
    const iv = crypto.getRandomValues(new Uint8Array(12));
    const ciphertext = await crypto.subtle.encrypt(
      { name: "AES-GCM", iv, additionalData: this.aad(metadata) },
      await this.key(),
      new TextEncoder().encode(JSON.stringify(secret)),
    );
    return {
      metadata,
      iv: Array.from(iv),
      ciphertext: Array.from(new Uint8Array(ciphertext)),
    };
  }
  private async decrypt(encrypted: EncryptedSecret): Promise<Secret> {
    const bytes = await crypto.subtle.decrypt(
      {
        name: "AES-GCM",
        iv: Uint8Array.from(encrypted.iv),
        additionalData: this.aad(encrypted.metadata),
      },
      await this.key(),
      Uint8Array.from(encrypted.ciphertext),
    );
    return JSON.parse(new TextDecoder().decode(bytes)) as Secret;
  }
}

export function validateCredentialPolicy(policy: CredentialPolicy): void {
  if (typeof policy.injectionLocation?.header !== "boolean")
    throw new Error("invalid credential injection policy");
  if (policy.networking.type === "limited") {
    for (const host of policy.networking.allowedHosts) {
      if (
        !/^[a-z0-9.-]+$/i.test(host) ||
        new URL(`https://${host}`).hostname !== host.toLowerCase()
      )
        throw new Error("invalid credential host");
    }
  } else if (policy.networking.type === "destinations") {
    for (const destination of policy.networking.allowedDestinations) {
      const value =
        destination.type === "origin" ? destination.origin : destination.url;
      const url = new URL(value);
      if (url.protocol !== "https:" || url.username || url.password || url.hash)
        throw new Error(
          "credential destinations require HTTPS without userinfo or fragments",
        );
      if (destination.type === "origin" && value !== url.origin)
        throw new Error(
          "credential origin must contain only scheme, host and port",
        );
    }
  } else throw new Error("invalid credential network policy");
}

export function credentialPermitted(
  policy: CredentialPolicy,
  destination: CredentialDestination,
): boolean {
  if (!policy.injectionLocation.header) return false;
  const url = new URL(
    destination.type === "origin" ? destination.origin : destination.url,
  );
  if (url.protocol !== "https:" || url.username || url.password || url.hash)
    return false;
  if (policy.networking.type === "limited")
    return policy.networking.allowedHosts.some(
      (host) => host.toLowerCase() === url.hostname.toLowerCase(),
    );
  return policy.networking.allowedDestinations.some((allowed) =>
    allowed.type === "origin"
      ? new URL(allowed.origin).origin === url.origin
      : destination.type === "url" && new URL(allowed.url).href === url.href,
  );
}
