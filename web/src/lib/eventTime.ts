// Event ids are UUIDv7, whose first 48 bits are the creation time in Unix
// milliseconds. That lets the conversation list show when each conversation was
// last active straight from its `latest_event_id`, with no extra request.
const UUID_V7_MS_HEX_LEN = 12;
// Sanity window so a non-v7 id (or a zeroed one) does not render as 1970 or a
// far-future date: only accept timestamps from 2020-01-01 onward.
const MIN_PLAUSIBLE_MS = 1_577_836_800_000;

export function eventIdToTimestamp(eventId: string | null): number | null {
  if (!eventId) {
    return null;
  }
  const hex = eventId.replace(/-/g, "").slice(0, UUID_V7_MS_HEX_LEN);
  if (hex.length < UUID_V7_MS_HEX_LEN || !/^[0-9a-f]+$/i.test(hex)) {
    return null;
  }
  const ms = parseInt(hex, 16);
  if (!Number.isFinite(ms) || ms < MIN_PLAUSIBLE_MS) {
    return null;
  }
  return ms;
}

const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;
const WEEK_MS = 7 * DAY_MS;

// Compact relative time for the conversation list: "now", "5m", "2h", "3d", "2w",
// then an absolute month/day once it is older than a few weeks. `nowMs` is passed
// in so the formatting stays pure and testable.
export function formatRelativeTime(ms: number, nowMs: number): string {
  const delta = Math.max(0, nowMs - ms);
  if (delta < MINUTE_MS) {
    return "now";
  }
  if (delta < HOUR_MS) {
    return `${Math.floor(delta / MINUTE_MS)}m`;
  }
  if (delta < DAY_MS) {
    return `${Math.floor(delta / HOUR_MS)}h`;
  }
  if (delta < WEEK_MS) {
    return `${Math.floor(delta / DAY_MS)}d`;
  }
  if (delta < 4 * WEEK_MS) {
    return `${Math.floor(delta / WEEK_MS)}w`;
  }
  return new Date(ms).toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}
