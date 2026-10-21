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
  | { type: "list"; prefix: string; after: string | null }
  | { type: "copy"; source: string; destination: string };

type Entry = { bytes: Uint8Array } | { blob: string };
const prefix = "runtime/";

export class Storage {
  private readonly locks = new Map<string, Promise<unknown>>();

  private async locked<T>(
    key: string,
    operation: () => Promise<T>,
  ): Promise<T> {
    const previous = this.locks.get(key) ?? Promise.resolve();
    const pending = previous.catch(() => {}).then(operation);
    this.locks.set(key, pending);
    try {
      return await pending;
    } finally {
      if (this.locks.get(key) === pending) this.locks.delete(key);
    }
  }
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
    const path = blob ? `${this.namespace}/${crypto.randomUUID()}` : undefined;
    const entry: Entry = path ? { blob: path } : { bytes };
    let previous: Entry | undefined;
    try {
      if (path) await this.blobs.put(path, bytes);
      // Read the replaced reference in the same transaction as its replacement.
      // The per-key lock also protects readers while R2 deletion is in flight.
      previous = await this.kv.transaction(async (txn) => {
        const previous = await txn.get<Entry>(prefix + key);
        await txn.put(prefix + key, entry);
        if (alarm === true) await txn.setAlarm(Date.now() + 180_000);
        else if (alarm === false) await txn.deleteAlarm();
        return previous;
      });
    } catch (error) {
      if (path) {
        try {
          await this.blobs.delete(path);
        } catch (cleanup) {
          throw new Error(
            `${String(error)}; blob cleanup failed: ${String(cleanup)}`,
            {
              cause: error,
            },
          );
        }
      }
      throw error;
    }
    if (previous && "blob" in previous) await this.removeBlob(previous.blob);
  }

  private async removeBlob(path: string): Promise<void> {
    try {
      await this.blobs.delete(path);
    } catch (error) {
      // The reference mutation is already committed. Reporting failure here
      // could reject a turn whose queue entry was durably accepted.
      console.error(
        "Committed storage mutation left an orphan blob",
        path,
        error,
      );
    }
  }

  async handle(operation: StorageOperation): Promise<unknown> {
    switch (operation.type) {
      case "put":
        await this.locked(operation.key, () =>
          this.put(
            operation.key,
            operation.bytes,
            operation.blob,
            operation.alarm,
          ),
        );
        return null;
      case "get": {
        return this.locked(operation.key, async () => {
          const entry = await this.kv.get<Entry>(prefix + operation.key);
          return entry ? this.read(entry) : null;
        });
      }
      case "list": {
        // Match object-store prefixes on path boundaries, excluding the prefix itself.
        const path = operation.prefix
          ? `${operation.prefix.replace(/\/$/, "")}/`
          : "";
        const entries = await this.kv.list<Entry>({
          prefix: prefix + path,
          ...(operation.after === null
            ? {}
            : { startAfter: prefix + operation.after }),
        });
        return Array.from(entries.keys(), (key) => key.slice(prefix.length));
      }
      case "delete": {
        await this.locked(operation.key, async () => {
          const entry = await this.kv.transaction(async (txn) => {
            const entry = await txn.get<Entry>(prefix + operation.key);
            await txn.delete(prefix + operation.key);
            return entry;
          });
          if (entry && "blob" in entry) await this.removeBlob(entry.blob);
        });
        return null;
      }
      case "copy": {
        const source = await this.locked(operation.source, async () => {
          const entry = await this.kv.get<Entry>(prefix + operation.source);
          if (!entry) throw new Error("copy source is missing");
          return { bytes: await this.read(entry), blob: "blob" in entry };
        });
        await this.locked(operation.destination, () =>
          this.put(operation.destination, source.bytes, source.blob),
        );
        return null;
      }
    }
  }
}
