import { describe, expect, it } from "vitest";

import type { TurnContext } from "./index";
import {
  compactToolResult,
  validateJsonSchema,
  validateToolDefinition,
} from "./tools";

describe("validateJsonSchema", () => {
  const schema = {
    type: "object",
    additionalProperties: false,
    properties: {
      name: { type: "string" },
      retries: { type: "number" },
      options: {
        type: "object",
        additionalProperties: false,
        properties: {
          timeoutMs: { type: ["number", "null"] },
        },
        required: ["timeoutMs"],
      },
    },
    required: ["name", "options"],
  };

  it("accepts values matching the schema", () => {
    expect(() =>
      validateJsonSchema(
        schema,
        { name: "curl", retries: 2, options: { timeoutMs: null } },
        "tool initialization",
      ),
    ).not.toThrow();
  });

  it("rejects type mismatches with the offending path", () => {
    expect(() =>
      validateJsonSchema(
        schema,
        { name: 42, options: { timeoutMs: 1 } },
        "tool initialization",
      ),
    ).toThrow("tool initialization.name does not match schema type string");
  });

  it("rejects extra keys when additionalProperties is false", () => {
    expect(() =>
      validateJsonSchema(
        schema,
        { name: "curl", options: { timeoutMs: 1 }, extra: true },
        "tool initialization",
      ),
    ).toThrow("tool initialization.extra is not allowed");
  });

  it("enforces required keys in nested objects", () => {
    expect(() =>
      validateJsonSchema(
        schema,
        { name: "curl", options: {} },
        "tool initialization",
      ),
    ).toThrow("tool initialization.options.timeoutMs is required");
  });
});

describe("validateToolDefinition", () => {
  const validParameters = {
    type: "object",
    additionalProperties: false,
    properties: {},
  };

  it("accepts a well-formed definition", () => {
    expect(() =>
      validateToolDefinition({
        name: "curl-tool_2",
        description: "Fetch a URL.",
        parameters: validParameters,
      }),
    ).not.toThrow();
  });

  it("rejects names with characters outside the allowed pattern", () => {
    expect(() =>
      validateToolDefinition({
        name: "bad name!",
        description: "Broken.",
        parameters: validParameters,
      }),
    ).toThrow(
      "tool definition.name must contain only letters, numbers, underscores, and dashes, and be at most 64 characters",
    );
  });

  it("rejects names longer than 64 characters", () => {
    expect(() =>
      validateToolDefinition({
        name: "a".repeat(65),
        description: "Too long.",
        parameters: validParameters,
      }),
    ).toThrow("at most 64 characters");
    expect(() =>
      validateToolDefinition({
        name: "a".repeat(64),
        description: "Exactly at the limit.",
        parameters: validParameters,
      }),
    ).not.toThrow();
  });

  it("rejects parameters whose type is not object", () => {
    expect(() =>
      validateToolDefinition({
        name: "curl",
        description: "Fetch.",
        parameters: { type: "string", additionalProperties: false },
      }),
    ).toThrow("tool definition.parameters.type must be object");
  });

  it("rejects parameters without additionalProperties false", () => {
    expect(() =>
      validateToolDefinition({
        name: "curl",
        description: "Fetch.",
        parameters: { type: "object", properties: {} },
      }),
    ).toThrow("tool definition.parameters.additionalProperties must be false");
  });
});

describe("compactToolResult", () => {
  it("keeps values at exactly the 8000-char inline limit", async () => {
    const result = "x".repeat(8_000);
    const compacted = (await compactToolResult(fakeTurnContext(), {
      toolCallId: "call_1",
      toolName: "echo",
      source: "library",
      result,
    })) as Record<string, unknown>;

    expect(compacted.truncated).toBe(false);
    expect(compacted.value).toBe(result);
  });

  it("nulls the inline value one char past the limit and cuts the preview", async () => {
    const result = "x".repeat(8_001);
    const compacted = (await compactToolResult(fakeTurnContext(), {
      toolCallId: "call_1",
      toolName: "echo",
      source: "library",
      result,
    })) as Record<string, unknown>;

    expect(compacted.truncated).toBe(true);
    expect(compacted.value).toBeNull();
    expect(compacted.preview).toBe(`${"x".repeat(4_000)}\n...[truncated]`);
  });

  it("does not append a truncation marker at exactly 4000 preview chars", async () => {
    const result = "y".repeat(4_000);
    const compacted = (await compactToolResult(fakeTurnContext(), {
      toolCallId: "call_1",
      toolName: "echo",
      source: "library",
      result,
    })) as Record<string, unknown>;

    expect(compacted.preview).toBe(result);
  });

  it("carries ok false through for failed results", async () => {
    const compacted = (await compactToolResult(fakeTurnContext(), {
      toolCallId: "call_1",
      toolName: "echo",
      source: "built_in",
      result: { ok: false, error: "boom" },
    })) as Record<string, unknown>;

    expect(compacted.ok).toBe(false);
    expect(compacted.value).toEqual({ ok: false, error: "boom" });
    expect(compacted.source).toBe("built_in");
  });
});

// Minimal TurnContext: compactToolResult only touches the turn artifact store.
function fakeTurnContext(): TurnContext {
  let artifactIndex = 0;
  return {
    streaming: false,
    exoharness: {
      current: {
        turn: {
          async writeArtifactText(args: { path: string; text: string }) {
            artifactIndex += 1;
            return {
              artifactId: `artifact-${artifactIndex}`,
              path: args.path,
              version: 1,
              createdAt: "2026-01-01T00:00:00Z",
              sizeBytes: args.text.length,
            };
          },
        },
      },
    },
  } as unknown as TurnContext;
}
