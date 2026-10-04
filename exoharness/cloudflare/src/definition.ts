import { parse } from "yaml";

export const DEFINITION_PATH = "managed-agents/agent.md";
export interface Definition {
  name: string;
  harness: "basic" | "codex";
  instructions: string;
  config: {
    model: string;
    credential?: string;
    base_url?: string;
    max_output_tokens?: number;
    max_tool_round_trips?: number;
  };
  askShell: boolean;
}

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

export function parseDefinition(source: string): Definition {
  source = source.replace(/^\uFEFF/, "").replaceAll("\r\n", "\n");
  const match = /^---\n([\s\S]*?)\n---(?:\n|$)([\s\S]*)$/.exec(source);
  if (!match) throw new Error("agent definition requires YAML frontmatter");
  const raw = fields(parse(match[1]), [
    "name",
    "harness",
    "config",
    "model",
    "permission_policy",
    "tool_policies",
    "resources",
    "mcp_servers",
    "tools",
    "adapters",
    "tool_creation",
  ]);
  if (raw.harness !== "basic" && raw.harness !== "codex")
    throw new Error(
      "the Cloudflare prototype supports basic and codex harnesses",
    );
  for (const name of ["resources", "mcp_servers", "tools", "adapters"]) {
    if (
      raw[name] !== undefined &&
      (!Array.isArray(raw[name]) || raw[name].length)
    )
      throw new Error(`${name} is not supported by the Cloudflare prototype`);
  }
  if (raw.tool_creation !== undefined && raw.tool_creation !== false)
    throw new Error("tool creation is not supported");
  if (raw.config !== undefined && raw.model !== undefined)
    throw new Error("specify config once");
  const config = fields(raw.config ?? raw.model, [
    "model",
    "name",
    "credential",
    "base_url",
    "max_output_tokens",
    "max_tool_round_trips",
  ]);
  const model = text(config.model ?? config.name, "model");
  if (config.base_url !== undefined) {
    const url = new URL(text(config.base_url, "base_url"));
    if (
      url.protocol !== "https:" ||
      url.username ||
      url.password ||
      url.hash ||
      url.search
    )
      throw new Error(
        "model base_url requires HTTPS without credentials, query or fragment",
      );
  }
  for (const name of ["max_output_tokens", "max_tool_round_trips"]) {
    if (
      config[name] !== undefined &&
      (!Number.isSafeInteger(config[name]) ||
        (config[name] as number) < (name === "max_tool_round_trips" ? 0 : 1))
    )
      throw new Error(`invalid ${name}`);
    if (raw.harness === "codex" && config[name] !== undefined)
      throw new Error(`Codex does not support ${name} in this prototype`);
  }
  const policy = (value: unknown): boolean => {
    const rawPolicy = fields(value, ["type"]);
    if (!["always_ask", "always_allow"].includes(rawPolicy.type as string))
      throw new Error("invalid permission policy");
    return rawPolicy.type === "always_ask";
  };
  const toolPolicies =
    raw.tool_policies === undefined ? {} : fields(raw.tool_policies, ["shell"]);
  // externalSandbox permits native file writes without an approval callback.
  // Reject a policy we cannot enforce, as the native Exo Codex harness does.
  if (
    raw.harness === "codex" &&
    ((raw.permission_policy !== undefined && policy(raw.permission_policy)) ||
      (toolPolicies.shell !== undefined && policy(toolPolicies.shell)))
  )
    throw new Error(
      "Codex always_ask policies are not supported in this prototype",
    );
  return {
    name: text(raw.name, "name"),
    harness: raw.harness,
    instructions: text(match[2].trim(), "instructions"),
    config: {
      model,
      credential:
        config.credential === undefined
          ? undefined
          : text(config.credential, "credential"),
      base_url: config.base_url as string | undefined,
      max_output_tokens: config.max_output_tokens as number | undefined,
      max_tool_round_trips: config.max_tool_round_trips as number | undefined,
    },
    askShell:
      toolPolicies.shell !== undefined
        ? policy(toolPolicies.shell)
        : raw.permission_policy === undefined
          ? false
          : policy(raw.permission_policy),
  };
}
