import type { WorkerInboundEvent } from "../protocol";

export function errorMessage(value: unknown): string {
  if (value instanceof Error) {
    return value.message;
  }
  try {
    return String(value);
  } catch {
    return "unknown error";
  }
}

// Gateway close codes discord.js will not reconnect from. Each is a
// configuration problem that a worker restart cannot fix. discord.js emits the
// Client "shardDisconnect" event only for these codes; every other close is
// retried internally and surfaces as "shardReconnecting" instead.
// https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes
const UNRECOVERABLE_CLOSE_CODES = new Map<number, string>([
  [4004, "authentication failed: the bot token is invalid"],
  [4010, "invalid shard"],
  [4011, "sharding required"],
  [4012, "invalid gateway API version"],
  [4013, "invalid gateway intents"],
  [
    4014,
    "disallowed gateway intents: enable the privileged intents in the Discord developer portal",
  ],
]);

export function describeCloseCode(code: number): string {
  const detail = UNRECOVERABLE_CLOSE_CODES.get(code);
  return detail ? `${detail} (code ${code})` : `gateway closed (code ${code})`;
}

export type ResilienceDeps = {
  emit: (event: WorkerInboundEvent) => void;
  exit: (code: number) => void;
};

export type ResilienceHandlers = {
  onUnhandledRejection: (reason: unknown) => void;
  onUncaughtException: (error: unknown) => void;
  onShardDisconnect: (code: number) => void;
  onShardError: (error: unknown) => void;
  onLoginFailure: (error: unknown) => void;
};

// The worker is a child process the adapter runner restarts on every exit
// (a fixed 5s delay, with no give-up). So it should exit only when a fresh
// start can plausibly help; for a failure a restart cannot fix it stays up and
// reports the cause rather than churning. The emit/exit side effects are
// injected so the decisions can be unit-tested without a real process or
// gateway.
export function createResilienceHandlers(
  deps: ResilienceDeps,
): ResilienceHandlers {
  return {
    onUnhandledRejection(reason) {
      // A rejection that escapes discord.js's own gateway recovery leaves the
      // worker in an unknown state, so surface it and exit for a clean restart.
      deps.emit({
        type: "error",
        message: `unhandled rejection: ${errorMessage(reason)}`,
      });
      deps.exit(1);
    },
    onUncaughtException(error) {
      deps.emit({
        type: "error",
        message: `uncaught exception: ${errorMessage(error)}`,
      });
      deps.exit(1);
    },
    onShardDisconnect(code) {
      // discord.js emits shardDisconnect only for close codes it will not
      // reconnect from, and all of them are configuration errors. A restart
      // cannot fix those and would just reconnect-storm Discord, so report the
      // cause and stay up instead of exiting into a 5s restart loop.
      deps.emit({ type: "disconnected", reason: describeCloseCode(code) });
    },
    onShardError(error) {
      // Shard errors are transient; discord.js keeps reconnecting, so report
      // the error without tearing the worker down.
      deps.emit({
        type: "error",
        message: `shard error: ${errorMessage(error)}`,
      });
    },
    onLoginFailure(error) {
      // Login can fail transiently, in which case a restart may succeed, so
      // surface the error and exit. A persistently bad token reports this on
      // each retry.
      deps.emit({
        type: "error",
        message: `discord login failed: ${errorMessage(error)}`,
      });
      deps.exit(1);
    },
  };
}
