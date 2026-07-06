import { describe, expect, it } from "vitest";

import {
  AsyncQueue,
  defaultServerRequestResult,
  linesFromChunks,
  tryParseProtocolMessage,
} from "./app-server";

async function* chunksOf(...chunks: string[]): AsyncGenerator<string> {
  for (const chunk of chunks) {
    yield chunk;
  }
}

async function collectLines(chunks: AsyncIterable<string>): Promise<string[]> {
  const lines: string[] = [];
  for await (const line of linesFromChunks(chunks)) {
    lines.push(line);
  }
  return lines;
}

describe("linesFromChunks", () => {
  it("splits newline-framed messages and drops empty lines", async () => {
    await expect(collectLines(chunksOf("a\nb\n\nc\n"))).resolves.toEqual([
      "a",
      "b",
      "c",
    ]);
  });

  it("strips CRLF line endings", async () => {
    await expect(
      collectLines(chunksOf('{"method":"x"}\r\n{"id":1}\r\n')),
    ).resolves.toEqual(['{"method":"x"}', '{"id":1}']);
  });

  it("buffers partial lines across chunk boundaries", async () => {
    await expect(
      collectLines(chunksOf('{"met', 'hod":"x"}\n{"id"', ":1}\n")),
    ).resolves.toEqual(['{"method":"x"}', '{"id":1}']);
  });

  it("flushes a trailing line without a final newline", async () => {
    await expect(
      collectLines(chunksOf('{"id":1}\n{"id"', ":2}")),
    ).resolves.toEqual(['{"id":1}', '{"id":2}']);
  });

  it("does not flush trailing whitespace", async () => {
    await expect(collectLines(chunksOf("a\n  "))).resolves.toEqual(["a"]);
  });
});

describe("tryParseProtocolMessage", () => {
  it("parses complete JSON objects", () => {
    expect(tryParseProtocolMessage('{"method":"turn/started"}')).toEqual({
      type: "message",
      message: { method: "turn/started" },
    });
  });

  it("buffers JSON truncated mid-string instead of throwing", () => {
    // A line cut inside a string value surfaces as incomplete so the read
    // loop keeps accumulating instead of crashing.
    expect(tryParseProtocolMessage('{"method":"turn/sta')).toEqual({
      type: "incomplete",
    });
    expect(
      tryParseProtocolMessage('{"method":"x","params":{"text":"partial chu'),
    ).toEqual({ type: "incomplete" });
  });

  it("still throws on JSON that is malformed rather than truncated", () => {
    expect(() => tryParseProtocolMessage("}{")).toThrow(SyntaxError);
  });

  it("rejects complete JSON that is not an object", () => {
    expect(() => tryParseProtocolMessage('"just a string"')).toThrow(
      'invalid codex app-server message: "just a string"',
    );
  });
});

describe("defaultServerRequestResult", () => {
  it("declines approval requests by default", () => {
    expect(
      defaultServerRequestResult("item/commandExecution/requestApproval"),
    ).toEqual({ decision: "decline" });
    expect(
      defaultServerRequestResult("item/fileChange/requestApproval"),
    ).toEqual({ decision: "decline" });
    expect(
      defaultServerRequestResult("item/permissions/requestApproval"),
    ).toEqual({ scope: "turn", permissions: {} });
    expect(defaultServerRequestResult("mcpServer/elicitation/request")).toEqual(
      { action: "decline", content: null },
    );
    expect(defaultServerRequestResult("item/tool/requestUserInput")).toEqual({
      action: "cancel",
      answers: {},
    });
    expect(defaultServerRequestResult("tool/requestUserInput")).toEqual({
      action: "cancel",
      answers: {},
    });
  });

  it("returns null for unknown server requests", () => {
    expect(defaultServerRequestResult("something/else")).toBeNull();
  });
});

describe("AsyncQueue", () => {
  it("delivers buffered values in push order", async () => {
    const queue = new AsyncQueue<string>();
    queue.push("a");
    queue.push("b");
    await expect(queue.next()).resolves.toEqual({ done: false, value: "a" });
    await expect(queue.next()).resolves.toEqual({ done: false, value: "b" });
  });

  it("resolves a waiting consumer when a value arrives", async () => {
    const queue = new AsyncQueue<string>();
    const pending = queue.next();
    queue.push("late");
    await expect(pending).resolves.toEqual({ done: false, value: "late" });
  });

  it("ends waiting and future consumers after end()", async () => {
    const queue = new AsyncQueue<string>();
    const pending = queue.next();
    queue.end();
    await expect(pending).resolves.toEqual({ done: true, value: undefined });
    await expect(queue.next()).resolves.toEqual({
      done: true,
      value: undefined,
    });
  });

  it("drains buffered values before reporting done", async () => {
    const queue = new AsyncQueue<string>();
    queue.push("queued");
    queue.end();
    await expect(queue.next()).resolves.toEqual({
      done: false,
      value: "queued",
    });
    await expect(queue.next()).resolves.toEqual({
      done: true,
      value: undefined,
    });
  });

  it("rejects waiting and future consumers after fail()", async () => {
    const queue = new AsyncQueue<string>();
    const pending = queue.next();
    queue.fail(new Error("transport died"));
    await expect(pending).rejects.toThrow("transport died");
    await expect(queue.next()).rejects.toThrow("transport died");
  });
});
