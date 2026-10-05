import { describe, expect, it } from "vitest";
import { WarmResourceCache } from "./shared";

describe("WarmResourceCache", () => {
  it("shares one replacement when concurrent callers find a dead process", async () => {
    const cache = new WarmResourceCache<{ running: boolean }>();
    const dead = { running: false };
    await cache.get("thread", async () => dead);
    let starts = 0;
    const create = async () => {
      starts++;
      return { running: true };
    };
    const [first, second] = await Promise.all([
      cache.get("thread", create, (process) => process.running),
      cache.get("thread", create, (process) => process.running),
    ]);
    expect(starts).toBe(1);
    expect(first.resource).toBe(second.resource);
  });

  it("does not evict a newer process when an old startup fails", async () => {
    const cache = new WarmResourceCache<object>();
    let fail!: (error: Error) => void;
    const old = cache.get(
      "thread",
      () =>
        new Promise((_resolve, reject) => {
          fail = reject;
        }),
    );
    await cache.delete("thread");
    const current = await cache.get("thread", async () => ({}));
    fail(new Error("old startup failed"));
    await expect(old).rejects.toThrow("old startup failed");
    const reused = await cache.get("thread", async () => ({}));
    expect(reused.reused).toBe(true);
    expect(reused.resource).toBe(current.resource);
  });
});
