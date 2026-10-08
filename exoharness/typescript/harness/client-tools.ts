import type { ContentBlock } from "@modelcontextprotocol/sdk/types.js";
import type { Event, JsonValue, Message, TurnContext } from "./index";

type ModelInput = Extract<Message, { role: "user" }>["content"];

export function toolResultFailed(result: JsonValue): boolean {
  return (
    result != null &&
    typeof result === "object" &&
    !Array.isArray(result) &&
    (result.ok === false || result.is_error === true)
  );
}

export function clientToolModelInput(event: Event) {
  if (
    event.data.type !== "custom" ||
    event.data.event_type !== "agent_runtime.frontend_tool_response"
  )
    return null;
  const response = event.data.payload as unknown as {
    tool_call_id: string;
    result: { type: string; model_input?: ModelInput | null };
  };
  return response.result.type === "frontend_tool_success" &&
    response.result.model_input != null
    ? { id: response.tool_call_id, content: response.result.model_input }
    : null;
}

export async function getClientToolModelInput(
  context: TurnContext,
  name: string,
  id: string,
): Promise<ModelInput | null> {
  if (!context.agentConfig.frontendTools?.some((tool) => tool.name === name))
    return null;
  let cursor: string | null = null;
  do {
    const page = await context.exoharness.current.conversation.getEvents({
      turnId: context.exoharness.current.turn.record.id,
      sessionId: context.exoharness.current.turn.record.sessionId,
      types: ["agent_runtime.frontend_tool_response"],
      direction: "asc",
      limit: 100,
      cursor,
    });
    for (const event of page.events) {
      const input = clientToolModelInput(event);
      if (input?.id === id) return input.content;
    }
    cursor = page.cursor ?? null;
  } while (cursor);
  return null;
}

export function clientModelInputContent(input: ModelInput): ContentBlock[] {
  if (typeof input === "string") return [{ type: "text", text: input }];
  return input.map((part): ContentBlock => {
    if (part.type === "text") return { type: "text", text: part.text };
    if (part.type === "image" && typeof part.image === "string") {
      const match = /^data:([^;,]+);base64,(.+)$/s.exec(part.image);
      if (match) return { type: "image", mimeType: match[1], data: match[2] };
    }
    throw new Error(
      "Client tool model input requires text or images supplied as base64 data URLs",
    );
  });
}
