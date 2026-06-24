import { describe, expect, it } from "vitest";
import { eventIdToTimestamp, formatRelativeTime } from "./eventTime";

describe("eventIdToTimestamp", () => {
  it("extracts the UUIDv7 millisecond timestamp", () => {
    // discord-clean's latest_event_id; created_at is 2026-06-23T18:43:54.142Z.
    const ms = eventIdToTimestamp("019ef5cb-ca9e-7d71-96f0-8ec5a57a4040");
    expect(ms).toBe(Date.parse("2026-06-23T18:43:54.142Z"));
  });

  it("returns null for empty, malformed, or implausibly old ids", () => {
    expect(eventIdToTimestamp(null)).toBeNull();
    expect(eventIdToTimestamp("")).toBeNull();
    expect(eventIdToTimestamp("not-a-uuid")).toBeNull();
    // All-zero prefix decodes to 1970, which is rejected by the sanity window.
    expect(
      eventIdToTimestamp("00000000-0000-7000-8000-000000000000"),
    ).toBeNull();
  });
});

describe("formatRelativeTime", () => {
  const now = Date.parse("2026-06-23T18:00:00.000Z");
  const ago = (ms: number) => formatRelativeTime(now - ms, now);

  it("formats sub-minute, minutes, hours, days, and weeks", () => {
    expect(ago(10_000)).toBe("now");
    expect(ago(5 * 60_000)).toBe("5m");
    expect(ago(2 * 3_600_000)).toBe("2h");
    expect(ago(3 * 86_400_000)).toBe("3d");
    expect(ago(2 * 7 * 86_400_000)).toBe("2w");
  });

  it("falls back to an absolute date past a month", () => {
    expect(ago(60 * 86_400_000)).toMatch(/[A-Za-z]{3}\s\d+/);
  });

  it("never shows a negative delta for a future id", () => {
    expect(formatRelativeTime(now + 5_000, now)).toBe("now");
  });
});
