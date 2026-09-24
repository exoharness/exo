import { type ResponseInput } from "openai/resources/responses/responses";
import { type JsonValue, type Message, toJsonValue } from "../harness";
import { linguaMessagesToResponsesInput } from "../model-runtime/responses";
import { isRecord } from "../model-runtime/shared";
import { type CodexNotification } from "./app-server";

interface CodexReplayServer {
  request(method: string, params: JsonValue): Promise<unknown>;
  events(): AsyncIterable<CodexNotification>;
}

type CodexReplayItem =
  | ResponseInput[number]
  | {
      type: "message";
      role: "assistant";
      content: { type: "output_text"; text: string }[];
    };

export function codexReplayItems(messages: Message[]): CodexReplayItem[] {
  return linguaMessagesToResponsesInput(
    messages.filter(
      (message) => message.role !== "system" && message.role !== "developer",
    ),
  ).map((item): CodexReplayItem => {
    if (item.type === "function_call") {
      return { ...item, name: item.name.replace(/[^a-zA-Z0-9_-]/g, "_") };
    }
    if (
      (item.type === undefined || item.type === "message") &&
      "role" in item
    ) {
      if (typeof item.content === "string") {
        if (item.role === "assistant") {
          return {
            ...item,
            type: "message",
            role: "assistant",
            content: [{ type: "output_text", text: item.content }],
          };
        }
        return {
          ...item,
          type: "message",
          content: [{ type: "input_text", text: item.content }],
        };
      }
      return { ...item, type: "message" };
    }
    return item;
  });
}

export function codexReplayChunks(
  items: CodexReplayItem[],
  maxChars = 64_000,
): CodexReplayItem[][] {
  const units: CodexReplayItem[][] = [];
  let unit: CodexReplayItem[] = [];
  const pendingCalls = new Set<string>();
  for (const item of items) {
    if (item.type === "function_call") pendingCalls.add(item.call_id);
    if (item.type === "function_call_output") pendingCalls.delete(item.call_id);
    unit.push(item);
    if (pendingCalls.size === 0) {
      units.push(unit);
      unit = [];
    }
  }
  if (unit.length > 0) units.push(unit);

  const chunks: CodexReplayItem[][] = [];
  let chunk: CodexReplayItem[] = [];
  let size = 0;
  for (const unit of units) {
    const unitSize = JSON.stringify(unit).length;
    if (chunk.length > 0 && size + unitSize > maxChars) {
      chunks.push(chunk);
      chunk = [];
      size = 0;
    }
    chunk.push(...unit);
    size += unitSize;
  }
  if (chunk.length > 0) chunks.push(chunk);
  return chunks;
}

export async function replayCodexHistory(
  server: CodexReplayServer,
  threadId: string,
  items: CodexReplayItem[],
): Promise<void> {
  const chunks = codexReplayChunks(items);
  for (let index = 0; index < chunks.length; index += 1) {
    await server.request("thread/inject_items", {
      threadId,
      items: toJsonValue(chunks[index]),
    });
    if (index + 1 < chunks.length) await compactThread(server, threadId);
  }
}

async function compactThread(
  server: CodexReplayServer,
  threadId: string,
): Promise<void> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      waitForCompaction(server, threadId),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(
          () => reject(new Error("Codex history compaction timed out")),
          120_000,
        );
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function waitForCompaction(
  server: CodexReplayServer,
  threadId: string,
): Promise<void> {
  await server.request("thread/compact/start", { threadId });
  for await (const notification of server.events()) {
    const params = notification.params;
    if (!isRecord(params) || params.threadId !== threadId) continue;
    if (notification.method === "error" && params.willRetry !== true) {
      throw new Error(
        `Codex history compaction failed: ${JSON.stringify(params.error)}`,
      );
    }
    if (notification.method !== "turn/completed" || !isRecord(params.turn))
      continue;
    if (params.turn.status !== "completed")
      throw new Error("Codex history compaction failed");
    return;
  }
  throw new Error("Codex stopped during history compaction");
}
