import {
  assistantTextMessage,
  defineHarness,
  validateToolPolicies,
  materializeConversationMessages,
  messageText,
  messagesToTranscript,
  messagesEvent,
  toolRequestedEvent,
  toolResultEvent,
  toJsonValue,
  toJsonObject,
  type JsonValue,
  type TurnContext,
} from "@exo/harness";
import {
  appendEvents,
  asRecord,
  pickEnv,
  resolveLlmBinding,
  WarmJsonlSandboxWorker,
} from "@exo/model-runtime/shared";

const PROVIDER_KEY_VARIABLES: Record<string, string> = {
  openai: "OPENAI_API_KEY",
  anthropic: "ANTHROPIC_API_KEY",
  google: "GEMINI_API_KEY",
};

const PI_EXTENSION = String.raw`
import { readFileSync } from "node:fs";

export default function (pi) {
  const tools = JSON.parse(readFileSync(new URL("./tools.json", import.meta.url), "utf8"));
  if (process.env.EXO_PI_BASE_URL) {
    pi.registerProvider(process.env.EXO_PI_PROVIDER, { baseUrl: process.env.EXO_PI_BASE_URL });
  }
  async function request(ctx, method, tool) {
    const value = await ctx.ui.input(method, JSON.stringify(tool));
    if (value === undefined) throw new Error("Exo tool request cancelled");
    const response = JSON.parse(value);
    if (!response.ok) throw new Error(response.error);
    return response.result;
  }
  pi.on("tool_call", async (event, ctx) => {
    if (tools.some(tool => tool.name === event.toolName)) return;
    try {
      await request(ctx, "exo.authorize_tool", { functionName: "pi." + event.toolName, arguments: event.input });
    } catch (error) {
      return { block: true, reason: String(error) };
    }
  });
  for (const tool of tools) {
    pi.registerTool({
      name: tool.name, label: tool.name, description: tool.description, parameters: tool.parameters,
      async execute(_id, args, _signal, _onUpdate, ctx) {
        const result = await request(ctx, "exo.execute_tool", { functionName: tool.name, arguments: args });
        if (result && (result.is_error === true || result.ok === false)) throw new Error(JSON.stringify(result));
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
      },
    });
  }
}
`;

function textContent(message: Record<string, unknown>): string {
  if (!Array.isArray(message.content)) return "";
  return message.content
    .map((part: unknown) => {
      const record = asRecord(part);
      return record.type === "text" && typeof record.text === "string"
        ? record.text
        : "";
    })
    .join("");
}

function usageRecord(
  message: Record<string, unknown>,
  provider: string,
): Record<string, JsonValue> | undefined {
  if (!message.usage) return undefined;
  const usage = asRecord(message.usage);
  const cost = asRecord(usage.cost);
  const record: Record<string, JsonValue> = {};
  if (typeof message.model === "string") record.model = message.model;
  if (typeof usage.input === "number") {
    record.prompt_tokens =
      provider === "anthropic"
        ? usage.input
        : usage.input +
          (typeof usage.cacheRead === "number" ? usage.cacheRead : 0) +
          (typeof usage.cacheWrite === "number" ? usage.cacheWrite : 0);
  }
  const fields: Array<[string, unknown]> = [
    ["completion_tokens", usage.output],
    ["prompt_cached_tokens", usage.cacheRead],
    ["prompt_cache_creation_tokens", usage.cacheWrite],
    ["cost_usd", cost.total],
  ];
  for (const [key, value] of fields) {
    if (typeof value === "number") record[key] = value;
  }
  return record;
}

export default defineHarness({
  nativeToolApprovals: true,
  async runTurn(context: TurnContext) {
    validateToolPolicies(context, [
      ...["read", "bash", "edit", "write", "grep", "find", "ls"].map(
        (name) => `pi.${name}`,
      ),
      ...context.tools.map((tool) => tool.name),
    ]);
    const history = await materializeConversationMessages(
      context.exoharness.current.conversation,
    );
    const prompt = messagesToTranscript(
      history.filter(
        (message) => message.role !== "system" && message.role !== "developer",
      ),
    );
    const binding = await resolveLlmBinding(context);
    const slash = binding.model.indexOf("/");
    const provider = slash < 0 ? "openai" : binding.model.slice(0, slash);
    const model = slash < 0 ? binding.model : binding.model.slice(slash + 1);
    const env = {
      ...pickEnv((key) => key.startsWith("PI_")),
      EXO_PI_PROVIDER: provider,
      ...(binding.baseUrl ? { EXO_PI_BASE_URL: binding.baseUrl } : {}),
      ...(binding.apiKey
        ? { [PROVIDER_KEY_VARIABLES[provider] ?? ""]: binding.apiKey }
        : {}),
    };
    if (binding.apiKey && !PROVIDER_KEY_VARIABLES[provider]) {
      throw new Error(
        `Pi API-key bindings are not configured for provider ${provider}`,
      );
    }
    const args = [
      "--mode",
      "rpc",
      "--no-session",
      "--no-extensions",
      "--provider",
      provider,
      "--model",
      model,
    ];
    const instructions = context.agentConfig.instructions
      .map(messageText)
      .filter(Boolean)
      .join("\n\n");
    if (instructions) args.push("--system-prompt", instructions);
    // Read one tools header line; leave the remaining RPC input for Pi.
    const process = await context.startSandboxProcess({
      command: [
        "sh",
        "-c",
        'set -eu; : "${HOME:?Pi sandbox image must define HOME}"; dir="$HOME/.pi/exo-$1"; shift; mkdir -p "$dir"; IFS= read -r tools; printf %s "$tools" > "$dir/tools.json"; printf %s "$1" > "$dir/exo.mjs"; shift; exec pi --extension "$dir/exo.mjs" "$@"',
        "exo-pi",
        context.exoharness.current.conversation.record.id,
        PI_EXTENSION,
        ...args,
      ],
      env,
    });
    const worker = new WarmJsonlSandboxWorker<
      JsonValue,
      Record<string, unknown>
    >({
      name: "Pi",
      process,
      parseEvent: (line) => asRecord(JSON.parse(line)),
    });
    let finalError: string | undefined;
    const activeTools = new Set<string>();
    try {
      await process.writeStdin(`${JSON.stringify(context.tools)}\n`);
      await worker.request(
        { type: "prompt", message: prompt },
        async (event) => {
          if (event.type === "extension_ui_request") {
            if (
              event.method !== "input" ||
              (event.title !== "exo.authorize_tool" &&
                event.title !== "exo.execute_tool")
            ) {
              if (
                ["select", "confirm", "input", "editor"].includes(
                  String(event.method),
                )
              ) {
                await process.writeStdin(
                  `${JSON.stringify({ type: "extension_ui_response", id: event.id, cancelled: true })}\n`,
                );
              }
              return;
            }
            let response: JsonValue;
            try {
              if (typeof event.placeholder !== "string")
                throw new Error("Pi tool request is missing its arguments");
              const request = asRecord(JSON.parse(event.placeholder));
              if (typeof request.functionName !== "string")
                throw new Error("Pi tool request is missing its name");
              const tool = {
                functionName: request.functionName,
                arguments: toJsonObject(request.arguments),
              };
              const result =
                event.title === "exo.execute_tool"
                  ? await context.executeTool(tool)
                  : (await context.authorizeTool(tool), null);
              response = { ok: true, result };
            } catch (error) {
              response = {
                ok: false,
                error: error instanceof Error ? error.message : String(error),
              };
            }
            await process.writeStdin(
              `${JSON.stringify({ type: "extension_ui_response", id: event.id, value: JSON.stringify(response) })}\n`,
            );
          } else if (event.type === "message_update") {
            const delta = asRecord(event.assistantMessageEvent);
            if (delta.type === "text_delta" && typeof delta.delta === "string")
              await context.stream.text(delta.delta);
          } else if (event.type === "tool_execution_start") {
            if (
              typeof event.toolCallId !== "string" ||
              typeof event.toolName !== "string"
            )
              throw new Error("Invalid Pi tool call");
            const name = context.tools.some(
              (tool) => tool.name === event.toolName,
            )
              ? event.toolName
              : `pi.${event.toolName}`;
            activeTools.add(event.toolCallId);
            const args = toJsonObject(event.args);
            await appendEvents(context, [
              toolRequestedEvent({
                toolCallId: event.toolCallId,
                request: { functionName: name, arguments: args },
              }),
            ]);
          } else if (event.type === "tool_execution_end") {
            if (
              typeof event.toolCallId !== "string" ||
              !activeTools.has(event.toolCallId)
            )
              throw new Error("Invalid Pi tool result");
            const result = {
              result: toJsonValue(event.result),
              is_error: event.isError === true,
            };
            await appendEvents(context, [
              toolResultEvent(event.toolCallId, result),
            ]);
            activeTools.delete(event.toolCallId);
          } else if (event.type === "message_end") {
            const message = asRecord(event.message);
            if (message.role === "assistant") {
              const text = textContent(message);
              await appendEvents(context, [
                messagesEvent(
                  text ? [assistantTextMessage(text)] : [],
                  undefined,
                  usageRecord(message, provider),
                ),
              ]);
              finalError =
                message.stopReason === "error" ||
                message.stopReason === "aborted"
                  ? String(
                      message.errorMessage ??
                        `Pi stopped: ${message.stopReason}`,
                    )
                  : undefined;
            }
          } else if (event.type === "response" && event.success === false) {
            throw new Error(String(event.error ?? "Pi RPC command failed"));
          } else if (event.type === "extension_error") {
            throw new Error(String(event.error ?? "Pi extension failed"));
          } else if (event.type === "agent_settled") {
            if (finalError) throw new Error(finalError);
            return true;
          }
        },
      );
    } finally {
      await worker.close();
    }
  },
});
