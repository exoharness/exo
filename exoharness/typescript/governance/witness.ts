import { createHash, timingSafeEqual } from "node:crypto";

/**
 * Witness record primitives — an optional, additive governance layer over the
 * substrate (proposal in exoharness/exo#215). The event log is append-only by
 * convention today; these primitives provide the enforced version: a
 * hash-chained, tamper-evident witness record that a sidecar can emit for
 * every event it observes on the stream, without touching the storage engine
 * or the default path.
 *
 * The record model is deliberately small: sequence number, timestamp,
 * canonical payload digest, and a link to the previous entry's digest. A
 * verifier can walk the chain from the genesis entry and detect any
 * modification, reordering, or omission after the fact.
 */

export interface WitnessEntry {
  /** Monotonic sequence number, starting at 1. */
  readonly seq: number;
  /** ISO-8601 timestamp recorded when the entry was appended. */
  readonly timestamp: string;
  /** SHA-256 hex digest of the canonical JSON of the recorded event. */
  readonly payloadDigest: string;
  /** SHA-256 hex digest of the previous entry, or GENESIS for the first. */
  readonly prevDigest: string;
  /** SHA-256 hex digest over this entry's own canonical form. */
  readonly digest: string;
}

/** Digest placeholder for the entry that starts a chain. */
export const GENESIS_DIGEST = "0".repeat(64);

function sha256(input: string): string {
  return createHash("sha256").update(input, "utf8").digest("hex");
}

/**
 * Canonical JSON: object keys sorted recursively, arrays preserved, no
 * insignificant whitespace. Required so that logically identical events
 * always hash identically regardless of key insertion order.
 */
export function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") {
    return JSON.stringify(value) ?? "null";
  }
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(",")}]`;
  }
  const entries = Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v !== undefined)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
    .map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`);
  return `{${entries.join(",")}}`;
}

export function digestEvent(event: unknown): string {
  return sha256(canonicalJson(event));
}

/**
 * Append an event to the chain. Pure: returns the new entry without mutating
 * anything, so callers can back it with any store (file, SQLite, sidecar).
 * `now` is injectable for deterministic tests.
 */
export function append(
  prev: WitnessEntry | null,
  event: unknown,
  now: () => Date = () => new Date(),
): WitnessEntry {
  const seq = (prev?.seq ?? 0) + 1;
  const prevDigest = prev?.digest ?? GENESIS_DIGEST;
  const timestamp = now().toISOString();
  const payloadDigest = digestEvent(event);
  const digest = sha256(
    canonicalJson({ seq, timestamp, payloadDigest, prevDigest }),
  );
  return { seq, timestamp, payloadDigest, prevDigest, digest };
}

export interface VerifyResult {
  readonly ok: boolean;
  /** Sequence number where verification failed, if it did. */
  readonly failedAt?: number;
  readonly reason?: string;
}

function digestsEqual(a: string, b: string): boolean {
  const bufA = Buffer.from(a, "hex");
  const bufB = Buffer.from(b, "hex");
  return bufA.length === bufB.length && timingSafeEqual(bufA, bufB);
}

function recomputeDigest(entry: WitnessEntry): string {
  return sha256(
    canonicalJson({
      seq: entry.seq,
      timestamp: entry.timestamp,
      payloadDigest: entry.payloadDigest,
      prevDigest: entry.prevDigest,
    }),
  );
}

/**
 * Verify a full chain from genesis: sequence numbers must be contiguous from
 * 1, each entry must link to its predecessor, and each digest must match the
 * entry's recomputed hash. Any single rewritten, reordered, or removed entry
 * breaks the chain here.
 */
export function verify(chain: readonly WitnessEntry[]): VerifyResult {
  let prev: WitnessEntry | null = null;
  for (let i = 0; i < chain.length; i++) {
    const entry = chain[i]!;
    const expectedSeq = i + 1;
    if (entry.seq !== expectedSeq) {
      return {
        ok: false,
        failedAt: entry.seq,
        reason: `expected seq ${expectedSeq}, got ${entry.seq}`,
      };
    }
    const expectedPrev = prev?.digest ?? GENESIS_DIGEST;
    if (!digestsEqual(entry.prevDigest, expectedPrev)) {
      return {
        ok: false,
        failedAt: entry.seq,
        reason: "prevDigest does not link to predecessor",
      };
    }
    if (!digestsEqual(entry.digest, recomputeDigest(entry))) {
      return {
        ok: false,
        failedAt: entry.seq,
        reason: "digest does not match entry contents",
      };
    }
    prev = entry;
  }
  return { ok: true };
}
