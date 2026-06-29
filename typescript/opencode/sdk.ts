import net from "node:net";

import { createOpencode, type Config } from "@opencode-ai/sdk";

import { opencodeModelRef, type OpencodeModelRef } from "./protocol";

type OpencodeInstance = Awaited<ReturnType<typeof createOpencode>>;

const OPENCODE_SERVER_START_ATTEMPTS = 4;

export interface ObservedTool {
  callId: string;
  name: string;
  status: "running" | "completed" | "error";
  args?: unknown;
  result?: unknown;
}

export interface OpencodeSdkRunnerOptions {
  model: string;
  cwd: string;
  apiKey?: string;
  baseUrl?: string;
  provider?: string;
  title?: string;
  onRunStarted?: (args: { sessionID: string }) => Promise<void> | void;
  onDelta?: (text: string) => Promise<void> | void;
  onTool?: (tool: ObservedTool) => Promise<void> | void;
  onMessage?: (message: unknown) => Promise<void> | void;
}

export interface OpencodeSdkRunResult {
  sessionID: string;
  status: "finished" | "error";
  result: string;
  model: { providerID: string; modelID: string };
  durationMs: number;
}

/**
 * Hosts an opencode server (and its client) inside the sandbox. The server is
 * kept warm across turns; a fresh session is created per turn because the host
 * harness injects the full exoharness transcript into every prompt.
 */
export class OpencodeSdkSession {
  private instance: OpencodeInstance | null = null;
  private key: string | null = null;

  async run(
    prompt: string,
    options: OpencodeSdkRunnerOptions,
  ): Promise<OpencodeSdkRunResult> {
    const ref = opencodeModelRef(options.model, options.provider);
    const key = opencodeServerKey(ref, options);
    if (this.instance && this.key !== key) {
      await this.close();
    }
    if (!this.instance) {
      this.instance = await startOpencodeServer(buildConfig(ref, options));
      this.key = key;
    }

    const { client } = this.instance;
    const startedAt = Date.now();
    try {
      const created = await client.session.create({
        body: { title: options.title ?? "exo" },
        query: { directory: options.cwd },
      });
      if (created.error || !created.data) {
        throw new Error(
          `opencode session.create failed: ${describeError(created.error)}`,
        );
      }
      const sessionID = created.data.id;
      await options.onRunStarted?.({ sessionID });

      const response = await client.session.prompt({
        path: { id: sessionID },
        query: { directory: options.cwd },
        body: {
          model: { providerID: ref.providerID, modelID: ref.modelID },
          parts: [{ type: "text", text: prompt }],
        },
      });
      if (response.error || !response.data) {
        throw new Error(
          `opencode prompt failed: ${describeError(response.error)}`,
        );
      }

      // `prompt` only returns the final assistant message. Tool calls happen in
      // earlier messages of the agentic loop, so harvest the whole session to
      // project tool events; fall back to the prompt response if the listing
      // is unavailable.
      const messages = await client.session.messages({
        path: { id: sessionID },
        query: { directory: options.cwd },
      });
      const harvested =
        !messages.error && Array.isArray(messages.data)
          ? messages.data.flatMap((entry) =>
              Array.isArray(entry?.parts) ? entry.parts : [],
            )
          : Array.isArray(response.data.parts)
            ? response.data.parts
            : [];

      const seenTools = new Set<string>();
      for (const part of harvested) {
        await options.onMessage?.(part);
        if (isRecord(part) && part.type === "tool") {
          const id = firstString(part.callID, part.id) ?? "";
          if (id && seenTools.has(id)) {
            continue;
          }
          if (id) {
            seenTools.add(id);
          }
          await emitToolPart(part, options.onTool);
        }
      }

      // Final assistant text comes from the prompt response (the last message).
      const finalParts = Array.isArray(response.data.parts)
        ? response.data.parts
        : [];
      let finalText = "";
      for (const part of finalParts) {
        if (
          isRecord(part) &&
          part.type === "text" &&
          typeof part.text === "string"
        ) {
          finalText += part.text;
        }
      }
      if (finalText) {
        await options.onDelta?.(finalText);
      }

      const info = isRecord(response.data.info) ? response.data.info : null;
      const errored = Boolean(info && info.error);
      return {
        sessionID,
        status: errored ? "error" : "finished",
        result: finalText,
        model: { providerID: ref.providerID, modelID: ref.modelID },
        durationMs: Date.now() - startedAt,
      };
    } catch (error) {
      await this.close();
      throw error;
    }
  }

  async close(): Promise<void> {
    const instance = this.instance;
    this.instance = null;
    this.key = null;
    if (instance) {
      try {
        instance.server.close();
      } catch {
        // best effort; the sandbox process is torn down regardless.
      }
    }
  }
}

export function opencodeSdkErrorMessage(error: unknown): string {
  if (error instanceof Error) {
    return error.message;
  }
  return String(error);
}

async function emitToolPart(
  part: Record<string, unknown>,
  onTool: OpencodeSdkRunnerOptions["onTool"],
): Promise<void> {
  if (!onTool) {
    return;
  }
  const callId = firstString(part.callID, part.id) ?? "tool";
  const name = firstString(part.tool) ?? "unknown";
  const state = isRecord(part.state) ? part.state : {};
  const status = typeof state.status === "string" ? state.status : "completed";

  await onTool({ callId, name, status: "running", args: state.input });
  if (status === "completed") {
    await onTool({
      callId,
      name,
      status: "completed",
      result: state.output ?? null,
    });
  } else if (status === "error") {
    await onTool({
      callId,
      name,
      status: "error",
      result: state.error ?? state.output ?? null,
    });
  }
}

// opencode's server defaults to port 4096 and ignores `--port 0`, so a reused
// sandbox that already has a server bound there fails to start ("ServeError").
// Bind a fresh ephemeral port per server, retrying in case of a TOCTOU race.
async function startOpencodeServer(
  config: Config | undefined,
): Promise<OpencodeInstance> {
  let lastError: unknown;
  for (
    let attempt = 0;
    attempt < OPENCODE_SERVER_START_ATTEMPTS;
    attempt += 1
  ) {
    const port = await findFreePort();
    try {
      return await createOpencode({ hostname: "127.0.0.1", port, config });
    } catch (error) {
      lastError = error;
    }
  }
  throw new Error(
    `failed to start opencode server after ${OPENCODE_SERVER_START_ATTEMPTS} attempts: ${describeError(lastError)}`,
  );
}

function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = typeof address === "object" && address ? address.port : null;
      server.close(() => {
        if (port) {
          resolve(port);
        } else {
          reject(new Error("could not determine a free port"));
        }
      });
    });
  });
}

function buildConfig(
  ref: OpencodeModelRef,
  options: OpencodeSdkRunnerOptions,
): Config | undefined {
  const providerOptions: Record<string, unknown> = {};
  if (options.apiKey) {
    providerOptions.apiKey = options.apiKey;
  }
  if (options.baseUrl) {
    providerOptions.baseURL = options.baseUrl;
  }
  if (Object.keys(providerOptions).length === 0) {
    return undefined;
  }
  return {
    provider: {
      [ref.providerID]: { options: providerOptions },
    },
  } as unknown as Config;
}

function opencodeServerKey(
  ref: OpencodeModelRef,
  options: OpencodeSdkRunnerOptions,
): string {
  return JSON.stringify({
    provider: ref.providerID,
    api_key: options.apiKey ? "set" : "env",
    base_url: options.baseUrl ?? null,
  });
}

function describeError(error: unknown): string {
  if (error === undefined || error === null) {
    return "unknown error";
  }
  if (typeof error === "string") {
    return error;
  }
  if (isRecord(error) && typeof error.message === "string") {
    return error.message;
  }
  return JSON.stringify(error);
}

function firstString(...values: unknown[]): string | null {
  for (const value of values) {
    if (typeof value === "string" && value) {
      return value;
    }
  }
  return null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
