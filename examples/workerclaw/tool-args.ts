import type { JsonObject } from "@exo/harness";

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

/**
 * Unwrap nested `{ type: "valid", value: { ... } }` lingua/exo argument envelopes.
 * Some runtime paths double-wrap tool args; peel until a plain object remains.
 */
export function unwrapHarnessToolArgs(args: JsonObject): JsonObject {
  let current = args;
  while (
    current.type === "valid" &&
    isRecord(current.value) &&
    !Array.isArray(current.value)
  ) {
    current = current.value as JsonObject;
  }
  return current;
}
