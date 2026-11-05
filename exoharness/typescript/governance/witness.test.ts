import { describe, expect, it } from "vitest";
import {
  GENESIS_DIGEST,
  append,
  canonicalJson,
  digestEvent,
  verify,
  type WitnessEntry,
} from "./witness";

describe("canonicalJson", () => {
  it("is order-independent for object keys", () => {
    expect(canonicalJson({ b: 1, a: { d: 2, c: 3 } })).toBe(
      canonicalJson({ a: { c: 3, d: 2 }, b: 1 }),
    );
  });

  it("drops undefined values", () => {
    expect(canonicalJson({ a: 1, b: undefined })).toBe('{"a":1}');
  });

  it("preserves array order", () => {
    expect(canonicalJson([2, 1])).toBe("[2,1]");
    expect(canonicalJson([2, 1])).not.toBe(canonicalJson([1, 2]));
  });
});

describe("hash chain", () => {
  const tick = () => new Date(0);

  it("starts the chain at genesis", () => {
    const first = append(null, { kind: "Custom", seq: 1 }, tick);
    expect(first.seq).toBe(1);
    expect(first.prevDigest).toBe(GENESIS_DIGEST);
    expect(verify([first]).ok).toBe(true);
  });

  it("links entries via prevDigest", () => {
    let chain: WitnessEntry[] = [];
    for (const event of ["a", "b", "c"]) {
      chain = [...chain, append(chain.at(-1) ?? null, event, tick)];
    }
    expect(chain.map((e) => e.seq)).toEqual([1, 2, 3]);
    expect(chain[1]!.prevDigest).toBe(chain[0]!.digest);
    expect(chain[2]!.prevDigest).toBe(chain[1]!.digest);
    expect(verify(chain)).toEqual({ ok: true });
  });

  it("hashes identical events identically regardless of key order", () => {
    expect(digestEvent({ tool: "bash", args: { cmd: "ls" } })).toBe(
      digestEvent({ args: { cmd: "ls" }, tool: "bash" }),
    );
  });

  it("detects a rewritten payload", () => {
    const first = append(null, "original", tick);
    const forged: WitnessEntry = {
      ...first,
      payloadDigest: digestEvent("tampered"),
    };
    expect(verify([forged]).ok).toBe(false);
  });

  it("detects a dropped middle entry (resequencing gap)", () => {
    const a = append(null, "a", tick);
    const b = append(a, "b", tick);
    const c = append(b, "c", tick);
    // Attacker removes b and re-links c onto a.
    const relinked: WitnessEntry = { ...c, seq: 2, prevDigest: a.digest };
    const result = verify([a, relinked]);
    expect(result.ok).toBe(false);
  });

  it("detects reordering", () => {
    const a = append(null, "a", tick);
    const b = append(a, "b", tick);
    const c = append(b, "c", tick);
    expect(verify([a, c, b]).ok).toBe(false);
  });

  it("detects a forged entry appended after the head", () => {
    const a = append(null, "a", tick);
    const forged: WitnessEntry = { ...append(a, "b", tick), timestamp: "x" };
    expect(verify([a, forged]).ok).toBe(false);
  });

  it("accepts an empty chain", () => {
    expect(verify([])).toEqual({ ok: true });
  });
});
