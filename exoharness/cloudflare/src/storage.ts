/** Byte storage for Rust's object store. Values and paths remain opaque here. */
export type StorageOperation =
  | {
      type: "put";
      key: string;
      bytes: Uint8Array;
      blob: boolean;
      alarm: boolean | null;
    }
  | { type: "get" | "delete"; key: string }
  | { type: "list"; prefix: string }
  | { type: "copy"; source: string; destination: string };

type Entry = { bytes: Uint8Array } | { blob: string };
const prefix = "runtime/";

export class Storage {
  constructor(
    private readonly kv: DurableObjectStorage,
    private readonly blobs: R2Bucket,
    private readonly namespace: string,
  ) {}

  private async read(entry: Entry): Promise<Uint8Array> {
    if ("bytes" in entry) return entry.bytes;
    const blob = await this.blobs.get(entry.blob);
    if (!blob) throw new Error("stored blob is missing");
    return new Uint8Array(await blob.arrayBuffer());
  }

  private async put(
    key: string,
    bytes: Uint8Array,
    blob: boolean,
    alarm?: boolean | null,
  ): Promise<void> {
    const previous = await this.kv.get<Entry>(prefix + key);
    let entry: Entry = { bytes };
    if (blob) {
      const path = `${this.namespace}/${crypto.randomUUID()}`;
      await this.blobs.put(path, bytes);
      entry = { blob: path };
    }
    if (alarm === undefined || alarm === null) {
      await this.kv.put(prefix + key, entry);
    } else {
      // Persist the bytes and wakeup together, before Rust acknowledges a turn.
      await this.kv.transaction(async (txn) => {
        await txn.put(prefix + key, entry);
        if (alarm) await txn.setAlarm(Date.now() + 180_000);
        else await txn.deleteAlarm();
      });
    }
    if (previous && "blob" in previous) await this.blobs.delete(previous.blob);
  }

  async handle(operation: StorageOperation): Promise<unknown> {
    switch (operation.type) {
      case "put":
        await this.put(
          operation.key,
          operation.bytes,
          operation.blob,
          operation.alarm,
        );
        return null;
      case "get": {
        const entry = await this.kv.get<Entry>(prefix + operation.key);
        return entry ? this.read(entry) : null;
      }
      case "list": {
        // Match object-store prefixes on path boundaries, excluding the prefix itself.
        const path = operation.prefix
          ? `${operation.prefix.replace(/\/$/, "")}/`
          : "";
        const entries = await this.kv.list<Entry>({ prefix: prefix + path });
        return Array.from(entries.keys(), (key) => key.slice(prefix.length));
      }
      case "delete": {
        const entry = await this.kv.get<Entry>(prefix + operation.key);
        await this.kv.delete(prefix + operation.key);
        if (entry && "blob" in entry) await this.blobs.delete(entry.blob);
        return null;
      }
      case "copy": {
        const entry = await this.kv.get<Entry>(prefix + operation.source);
        if (!entry) throw new Error("copy source is missing");
        await this.put(
          operation.destination,
          await this.read(entry),
          "blob" in entry,
        );
        return null;
      }
    }
  }
}
