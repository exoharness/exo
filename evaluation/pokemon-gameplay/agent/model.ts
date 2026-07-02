// Minimal OpenAI Responses API client (plain fetch): vision input + function
// tools, stateless (full input list resent each round trip).

export interface ToolDefinition {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
}

export interface ToolCall {
  callId: string;
  name: string;
  arguments: Record<string, unknown>;
}

export interface ModelResponse {
  outputItems: unknown[]; // echoed back verbatim on the next round trip
  toolCalls: ToolCall[];
  text: string;
  inputTokens: number;
  outputTokens: number;
}

export interface ModelConfig {
  apiKey: string;
  model: string;
  reasoningEffort: string | null;
  maxOutputTokens: number;
}

export function textMessage(role: string, text: string): unknown {
  return { role, content: [{ type: "input_text", text }] };
}

export function imageMessage(text: string, pngBase64: string): unknown {
  return {
    role: "user",
    content: [
      { type: "input_text", text },
      {
        type: "input_image",
        image_url: `data:image/png;base64,${pngBase64}`,
        detail: "high",
      },
    ],
  };
}

export function functionCallOutput(callId: string, output: string): unknown {
  return { type: "function_call_output", call_id: callId, output };
}

export async function callModel(
  config: ModelConfig,
  input: unknown[],
  tools: ToolDefinition[],
): Promise<ModelResponse> {
  const body: Record<string, unknown> = {
    model: config.model,
    input,
    tools: tools.map((tool) => ({
      type: "function",
      name: tool.name,
      description: tool.description,
      parameters: tool.parameters,
      strict: false,
    })),
    max_output_tokens: config.maxOutputTokens,
    // Stateless usage: nothing persists server-side, so reasoning items must
    // carry encrypted content to be echoed back on the next round trip.
    store: false,
    include: ["reasoning.encrypted_content"],
  };
  if (config.reasoningEffort !== null) {
    body.reasoning = { effort: config.reasoningEffort };
  }

  let lastError = "";
  for (let attempt = 0; attempt < 3; attempt += 1) {
    const response = await fetch("https://api.openai.com/v1/responses", {
      method: "POST",
      headers: {
        Authorization: `Bearer ${config.apiKey}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(300_000),
    });
    const payload = (await response.json().catch(() => null)) as {
      output?: unknown[];
      usage?: { input_tokens?: number; output_tokens?: number };
      error?: { message?: string };
    } | null;
    if (response.ok && payload?.output !== undefined) {
      return parseResponse(payload as { output: unknown[]; usage?: never });
    }
    lastError = `model call failed (${response.status}): ${payload?.error?.message ?? "unknown error"}`;
    // Retry transient failures; 4xx other than 429 will not improve.
    if (response.status < 500 && response.status !== 429) {
      break;
    }
    await new Promise((resolve) => setTimeout(resolve, 2_000 * (attempt + 1)));
  }
  throw new Error(lastError);
}

function parseResponse(payload: {
  output: unknown[];
  usage?: { input_tokens?: number; output_tokens?: number };
}): ModelResponse {
  const toolCalls: ToolCall[] = [];
  const textParts: string[] = [];
  for (const item of payload.output) {
    if (item === null || typeof item !== "object") {
      continue;
    }
    const record = item as Record<string, unknown>;
    if (record.type === "function_call") {
      let args: Record<string, unknown> = {};
      try {
        const parsed = JSON.parse(String(record.arguments ?? "{}"));
        if (parsed !== null && typeof parsed === "object") {
          args = parsed as Record<string, unknown>;
        }
      } catch {
        // leave args empty; the tool will report the validation error
      }
      toolCalls.push({
        callId: String(record.call_id ?? ""),
        name: String(record.name ?? ""),
        arguments: args,
      });
    } else if (record.type === "message" && Array.isArray(record.content)) {
      for (const part of record.content) {
        const partRecord = part as Record<string, unknown>;
        if (partRecord?.type === "output_text") {
          textParts.push(String(partRecord.text ?? ""));
        }
      }
    }
  }
  return {
    // Strip server-side item ids: with store:false they reference items that
    // no longer exist, and resending them 404s the next round trip.
    outputItems: payload.output.map((item) => {
      if (item !== null && typeof item === "object" && "id" in item) {
        const { id: _id, ...rest } = item as Record<string, unknown>;
        return rest;
      }
      return item;
    }),
    toolCalls,
    text: textParts.join("\n").trim(),
    inputTokens: payload.usage?.input_tokens ?? 0,
    outputTokens: payload.usage?.output_tokens ?? 0,
  };
}
