import { describe, expect, it, vi } from "vitest";

import {
  createResilienceHandlers,
  describeCloseCode,
  errorMessage,
} from "./discord";

describe("errorMessage", () => {
  it("uses the message of an Error", () => {
    expect(errorMessage(new Error("boom"))).toBe("boom");
  });

  it("stringifies non-error values", () => {
    expect(errorMessage("plain")).toBe("plain");
    expect(errorMessage(undefined)).toBe("undefined");
    expect(errorMessage(42)).toBe("42");
  });

  it("never throws on values that cannot be stringified", () => {
    const hostile = {
      toString() {
        throw new Error("nope");
      },
    };
    expect(() => errorMessage(hostile)).not.toThrow();
    expect(errorMessage(hostile)).toBe("unknown error");
  });
});

describe("describeCloseCode", () => {
  it("explains the unrecoverable configuration codes", () => {
    expect(describeCloseCode(4004)).toContain("token is invalid");
    expect(describeCloseCode(4014)).toContain("privileged intents");
    expect(describeCloseCode(4014)).toContain("4014");
  });

  it("falls back for any other code", () => {
    expect(describeCloseCode(1006)).toBe("gateway closed (code 1006)");
  });
});

describe("createResilienceHandlers", () => {
  it("exits after an unhandled rejection", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    createResilienceHandlers({ emit, exit }).onUnhandledRejection(
      new Error("stray"),
    );
    expect(emit).toHaveBeenCalledWith({
      type: "error",
      message: "unhandled rejection: stray",
    });
    expect(exit).toHaveBeenCalledWith(1);
  });

  it("exits after an uncaught exception", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    createResilienceHandlers({ emit, exit }).onUncaughtException(
      new Error("bad state"),
    );
    expect(emit).toHaveBeenCalledWith({
      type: "error",
      message: "uncaught exception: bad state",
    });
    expect(exit).toHaveBeenCalledWith(1);
  });

  it("reports a shard disconnect without exiting into a restart loop", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    // 4014 (disallowed intents) is a config error a restart cannot fix; the
    // worker must stay up rather than churn.
    createResilienceHandlers({ emit, exit }).onShardDisconnect(4014);
    expect(emit).toHaveBeenCalledWith({
      type: "disconnected",
      reason: expect.stringContaining("privileged intents"),
    });
    expect(exit).not.toHaveBeenCalled();
  });

  it("keeps the worker alive on a shard error", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    createResilienceHandlers({ emit, exit }).onShardError(new Error("ws blip"));
    expect(emit).toHaveBeenCalledWith({
      type: "error",
      message: "shard error: ws blip",
    });
    expect(exit).not.toHaveBeenCalled();
  });

  it("exits when login fails", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    createResilienceHandlers({ emit, exit }).onLoginFailure(
      new Error("invalid token"),
    );
    expect(emit).toHaveBeenCalledWith({
      type: "error",
      message: "discord login failed: invalid token",
    });
    expect(exit).toHaveBeenCalledWith(1);
  });
});
