import {
  toolResultMessage,
  type Conversation,
  type Event,
  type EventData,
  type JsonObject,
  type Message,
  type ToolResult,
} from "@exo/harness";

/**
 * WorkerClaw-local conversation materialization.
 *
 * exo's event log often splits one model turn into multiple consecutive
 * assistant messages (text, then one message per tool_call). The Chat API
 * expects a single assistant message with all tool_calls, followed by tool
 * messages. We coalesce, then repair missing/orphan pairings before each call.
 */

export async function materializeWorkerclawPromptMessages(
  conversation: Conversation,
  instructions: Message[],
): Promise<Message[]> {
  const history = await materializeWorkerclawConversationMessages(conversation);
  return [...instructions, ...repairLinguaToolPairing(history)];
}

export async function materializeWorkerclawConversationMessages(
  conversation: Conversation,
): Promise<Message[]> {
  const result = await conversation.getEvents({
    direction: "asc",
    types: ["messages", "tool_requested", "tool_result"],
  });
  return materializeWorkerclawEventsToMessages(result.events);
}

export function materializeWorkerclawEventsToMessages(
  events: Event[],
): Message[] {
  const messages: Message[] = [];
  const toolCallNames = new Map<string, string>();
  const pendingToolCallIds: string[] = [];

  for (const event of events) {
    extendWorkerclawMaterializedMessages(
      messages,
      toolCallNames,
      pendingToolCallIds,
      event,
    );
  }
  flushDanglingToolResults(messages, toolCallNames, pendingToolCallIds);
  return messages;
}

/** @internal exported for tests */
export function repairLinguaToolPairing(messages: Message[]): Message[] {
  return normalizeToolRoundPairing(
    coalesceConsecutiveAssistantMessages(messages),
  );
}

/**
 * Anthropic/OpenAI-compatible APIs emit parallel tool calls as separate
 * assistant rows in exo's messages events. Merge them before tool results.
 */
function coalesceConsecutiveAssistantMessages(messages: Message[]): Message[] {
  const out: Message[] = [];

  for (const msg of messages) {
    const last = out[out.length - 1];
    if (msg.role === "assistant" && last?.role === "assistant") {
      last.content = mergeMessageContent(last.content, msg.content);
      if (msg.id) {
        last.id = msg.id;
      }
      continue;
    }
    out.push({ ...msg });
  }

  return out;
}

function normalizeToolRoundPairing(messages: Message[]): Message[] {
  const out: Message[] = [];
  let synthesized = 0;
  let dropped = 0;

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i]!;
    if (msg.role === "tool") {
      continue;
    }

    out.push(msg);

    const calls = toolCallsFromAssistant(msg);
    if (calls.length === 0) {
      continue;
    }

    const callIds = new Set(calls.map((call) => call.id));
    const needed = new Map(calls.map((call) => [call.id, call.name]));

    let j = i + 1;
    while (j < messages.length && messages[j]!.role === "tool") {
      const toolMsg = messages[j]!;
      const keptContent = filterToolResultContent(toolMsg, callIds, () => {
        dropped++;
      });
      if (keptContent.length > 0) {
        out.push({ role: "tool", content: keptContent });
      }
      for (const id of toolResultIdsFromParts(keptContent)) {
        needed.delete(id);
      }
      j++;
    }
    i = j - 1;

    for (const [id, name] of needed) {
      out.push(
        toolResultMessage(id, name, {
          ok: false,
          error:
            "tool result missing from event log; synthesized by WorkerClaw",
        }),
      );
      synthesized++;
    }
  }

  if (synthesized > 0 || dropped > 0) {
    console.warn(
      `[workerclaw] repaired tool pairing: synthesized=${synthesized} dropped_orphans=${dropped}`,
    );
  }

  return out;
}

function filterToolResultContent(
  toolMsg: Message,
  allowedCallIds: Set<string>,
  onDrop: () => void,
): unknown[] {
  if (!Array.isArray(toolMsg.content)) {
    return [];
  }
  const kept: unknown[] = [];
  for (const part of toolMsg.content) {
    if (!part || typeof part !== "object") {
      kept.push(part);
      continue;
    }
    const p = part as { type?: string; tool_call_id?: string };
    if (p.type === "tool_result") {
      if (
        typeof p.tool_call_id === "string" &&
        allowedCallIds.has(p.tool_call_id)
      ) {
        kept.push(part);
      } else {
        onDrop();
      }
      continue;
    }
    kept.push(part);
  }
  return kept;
}

function mergeMessageContent(existing: unknown, incoming: unknown): unknown[] {
  return [...contentParts(existing), ...contentParts(incoming)];
}

function contentParts(content: unknown): unknown[] {
  if (Array.isArray(content)) {
    return [...content];
  }
  if (typeof content === "string" && content.length > 0) {
    return [{ type: "text", text: content }];
  }
  return [];
}

function toolResultIdsFromParts(content: unknown[]): string[] {
  const ids: string[] = [];
  for (const part of content) {
    if (!part || typeof part !== "object") {
      continue;
    }
    const p = part as { type?: string; tool_call_id?: string };
    if (p.type === "tool_result" && typeof p.tool_call_id === "string") {
      ids.push(p.tool_call_id);
    }
  }
  return ids;
}

function extendWorkerclawMaterializedMessages(
  messages: Message[],
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
  event: Event,
): void {
  if (isMessagesEvent(event.data)) {
    flushDanglingToolResults(messages, toolCallNames, pendingToolCallIds);
    for (const message of event.data.messages) {
      registerToolCallsInMessage(message, toolCallNames, pendingToolCallIds);
    }
    messages.push(...event.data.messages);
    return;
  }

  if (isToolRequestedEvent(event.data)) {
    toolCallNames.set(
      event.data.tool_call_id,
      event.data.request.function_name,
    );
    pushPendingToolCall(pendingToolCallIds, event.data.tool_call_id);
    return;
  }

  if (isToolResultEvent(event.data)) {
    const toolName =
      toolCallNames.get(event.data.tool_call_id) ??
      findToolNameInMessages(messages, event.data.tool_call_id) ??
      "unknown";
    toolCallNames.set(event.data.tool_call_id, toolName);
    removePendingToolCall(pendingToolCallIds, event.data.tool_call_id);
    messages.push(
      toolResultMessage(event.data.tool_call_id, toolName, event.data.result),
    );
  }
}

function registerToolCallsInMessage(
  message: Message,
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
): void {
  for (const call of toolCallsFromAssistant(message)) {
    toolCallNames.set(call.id, call.name);
    pushPendingToolCall(pendingToolCallIds, call.id);
  }
}

function toolCallsFromAssistant(
  message: Message,
): Array<{ id: string; name: string }> {
  if (message.role !== "assistant" || !Array.isArray(message.content)) {
    return [];
  }
  const calls: Array<{ id: string; name: string }> = [];
  for (const part of message.content) {
    if (!part || typeof part !== "object") {
      continue;
    }
    const p = part as {
      type?: string;
      tool_call_id?: string;
      tool_name?: string;
    };
    if (
      p.type === "tool_call" &&
      typeof p.tool_call_id === "string" &&
      typeof p.tool_name === "string"
    ) {
      calls.push({ id: p.tool_call_id, name: p.tool_name });
    }
  }
  return calls;
}

function findToolNameInMessages(
  messages: Message[],
  toolCallId: string,
): string | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    for (const call of toolCallsFromAssistant(messages[i]!)) {
      if (call.id === toolCallId) {
        return call.name;
      }
    }
  }
  return null;
}

function flushDanglingToolResults(
  messages: Message[],
  toolCallNames: Map<string, string>,
  pendingToolCallIds: string[],
): void {
  while (pendingToolCallIds.length > 0) {
    const toolCallId = pendingToolCallIds.shift();
    if (!toolCallId) {
      continue;
    }
    const toolName = toolCallNames.get(toolCallId);
    if (!toolName) {
      continue;
    }
    messages.push(
      toolResultMessage(toolCallId, toolName, {
        ok: false,
        error: "tool execution did not complete before the previous turn ended",
      } satisfies ToolResult),
    );
  }
}

function pushPendingToolCall(pendingToolCallIds: string[], toolCallId: string) {
  if (!pendingToolCallIds.includes(toolCallId)) {
    pendingToolCallIds.push(toolCallId);
  }
}

function removePendingToolCall(
  pendingToolCallIds: string[],
  toolCallId: string,
): void {
  const index = pendingToolCallIds.indexOf(toolCallId);
  if (index >= 0) {
    pendingToolCallIds.splice(index, 1);
  }
}

function isMessagesEvent(
  data: EventData,
): data is EventData & { type: "messages"; messages: Message[] } {
  return data.type === "messages" && Array.isArray(data.messages);
}

function isToolRequestedEvent(data: EventData): data is EventData & {
  type: "tool_requested";
  tool_call_id: string;
  request: { function_name: string; arguments: JsonObject };
} {
  if (data.type !== "tool_requested") {
    return false;
  }
  if (typeof data.tool_call_id !== "string") {
    return false;
  }
  if (!data.request || typeof data.request !== "object") {
    return false;
  }
  return (
    typeof (data.request as { function_name?: unknown }).function_name ===
    "string"
  );
}

function isToolResultEvent(data: EventData): data is EventData & {
  type: "tool_result";
  tool_call_id: string;
  result: ToolResult;
} {
  return data.type === "tool_result" && typeof data.tool_call_id === "string";
}
