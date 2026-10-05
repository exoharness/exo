export const DEFINITION_PATH = "managed-agents/agent.md";
export function object(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value))
    throw new Error("expected an object");
  return value as Record<string, unknown>;
}
export function fields(
  value: unknown,
  names: string[],
): Record<string, unknown> {
  const record = object(value);
  for (const key of Object.keys(record))
    if (!names.includes(key)) throw new Error(`unsupported field: ${key}`);
  return record;
}
export function text(value: unknown, name: string): string {
  if (typeof value !== "string" || !value.trim())
    throw new Error(`${name} must be a nonempty string`);
  return value;
}
