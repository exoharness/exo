import { afterEach, describe, expect, it, vi } from "vitest";

import {
  createResilienceHandlers,
  describeCloseCode,
  errorMessage,
  startConnectionWatchdog,
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

  it("reports TLS access denied exceptions without exiting", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    const error = Object.assign(
      new Error("write EPROTO tlsv1 alert access denied"),
      { code: "EPROTO" },
    );
    createResilienceHandlers({ emit, exit }).onUncaughtException(error);
    expect(emit).toHaveBeenCalledWith({
      type: "error",
      message:
        "Discord TLS stream error: write EPROTO tlsv1 alert access denied",
    });
    expect(exit).not.toHaveBeenCalled();
  });

  it("exits after a shard disconnect instead of lingering as a zombie", () => {
    const emit = vi.fn();
    const exit = vi.fn();
    // shardDisconnect means discord.js gave up reconnecting; a worker that
    // stays up will never receive another message. The runner's exponential
    // backoff bounds the restart churn for persistent config errors.
    createResilienceHandlers({ emit, exit }).onShardDisconnect(4014);
    expect(emit).toHaveBeenCalledWith({
      type: "disconnected",
      reason: expect.stringContaining("privileged intents"),
    });
    expect(exit).toHaveBeenCalledWith(1);
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

describe("startConnectionWatchdog", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("exits when the client stays not-ready past the timeout", () => {
    vi.useFakeTimers();
    const emit = vi.fn();
    const exit = vi.fn();
    const stop = startConnectionWatchdog({
      isReady: () => false,
      emit,
      exit,
      intervalMs: 1_000,
      timeoutMs: 5_000,
    });
    vi.advanceTimersByTime(4_000);
    expect(exit).not.toHaveBeenCalled();
    vi.advanceTimersByTime(2_000);
    expect(emit).toHaveBeenCalledWith({
      type: "error",
      message: expect.stringContaining("discord gateway not ready"),
    });
    expect(exit).toHaveBeenCalledWith(1);
    stop();
  });

  it("stays quiet while the client is ready and recovers after blips", () => {
    vi.useFakeTimers();
    const emit = vi.fn();
    const exit = vi.fn();
    let ready = true;
    const stop = startConnectionWatchdog({
      isReady: () => ready,
      emit,
      exit,
      intervalMs: 1_000,
      timeoutMs: 5_000,
    });
    vi.advanceTimersByTime(60_000);
    expect(exit).not.toHaveBeenCalled();
    // A short blip below the timeout must not kill the worker.
    ready = false;
    vi.advanceTimersByTime(3_000);
    ready = true;
    vi.advanceTimersByTime(60_000);
    expect(exit).not.toHaveBeenCalled();
    expect(emit).not.toHaveBeenCalled();
    stop();
  });
});
