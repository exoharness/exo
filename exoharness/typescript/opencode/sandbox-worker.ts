import readline from "node:readline/promises";

import { OpencodeSdkSession, opencodeSdkErrorMessage } from "./sdk";
import {
  toOpencodeJson,
  type OpencodeWorkerEvent,
  type OpencodeWorkerRequest,
} from "./protocol";

async function main(): Promise<void> {
  const session = new OpencodeSdkSession();
  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity,
  });
  try {
    for await (const line of rl) {
      if (line.trim()) {
        await handleRequest(session, parseRequest(line));
      }
    }
  } finally {
    rl.close();
    await session.close();
  }
}

async function handleRequest(
  session: OpencodeSdkSession,
  request: OpencodeWorkerRequest,
): Promise<void> {
  await session
    .run(request.prompt, {
      model: request.model,
      cwd: request.cwd,
      apiKey: request.apiKey ?? process.env.OPENCODE_API_KEY,
      baseUrl: request.baseUrl,
      provider: request.provider ?? process.env.EXO_OPENCODE_PROVIDER,
      title: request.title,
      onRunStarted: ({ sessionID }) => {
        writeEvent({ type: "run_started", sessionID });
      },
      onDelta: (text) => {
        writeEvent({ type: "delta", text });
      },
      onTool: (tool) => {
        writeEvent({
          type: "tool",
          callId: tool.callId,
          name: tool.name,
          status: tool.status,
          args: toOpencodeJson(tool.args),
          result: toOpencodeJson(tool.result),
        });
      },
      onMessage: (message) => {
        writeEvent({ type: "message", message: toOpencodeJson(message) });
      },
    })
    .then((run) => {
      writeEvent({
        type: "completed",
        result: {
          id: run.sessionID,
          status: run.status,
          result: run.result,
          model: toOpencodeJson(run.model),
          durationMs: run.durationMs,
        },
      });
    })
    .catch((error: unknown) => {
      writeEvent({
        type: "error",
        message: opencodeSdkErrorMessage(error),
        error: toOpencodeJson(error),
      });
    });
}

function parseRequest(line: string): OpencodeWorkerRequest {
  const parsed = JSON.parse(line) as unknown;
  if (!isRecord(parsed)) {
    throw new Error("opencode sandbox worker request must be a JSON object");
  }
  if (typeof parsed.prompt !== "string") {
    throw new Error("opencode sandbox worker request requires prompt");
  }
  if (typeof parsed.model !== "string") {
    throw new Error("opencode sandbox worker request requires model");
  }
  if (typeof parsed.cwd !== "string") {
    throw new Error("opencode sandbox worker request requires cwd");
  }
  return {
    prompt: parsed.prompt,
    model: parsed.model,
    cwd: parsed.cwd,
    apiKey: typeof parsed.apiKey === "string" ? parsed.apiKey : undefined,
    baseUrl: typeof parsed.baseUrl === "string" ? parsed.baseUrl : undefined,
    provider: typeof parsed.provider === "string" ? parsed.provider : undefined,
    title: typeof parsed.title === "string" ? parsed.title : undefined,
  };
}

function writeEvent(event: OpencodeWorkerEvent): void {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

void main().catch((error: unknown) => {
  writeEvent({
    type: "error",
    message: opencodeSdkErrorMessage(error),
    error: toOpencodeJson(error),
  });
  process.exitCode = 1;
});
