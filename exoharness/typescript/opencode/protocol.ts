import type { JsonValue } from "@exo/harness";

export interface OpencodeWorkerRequest {
  prompt: string;
  model: string;
  cwd: string;
  apiKey?: string;
  baseUrl?: string;
  provider?: string;
  title?: string;
}

export type OpencodeWorkerEvent =
  | {
      type: "run_started";
      sessionID: string;
    }
  | {
      type: "delta";
      text: string;
    }
  | {
      type: "tool";
      callId: string;
      name: string;
      status: "running" | "completed" | "error";
      args?: JsonValue;
      result?: JsonValue;
    }
  | {
      type: "message";
      message: JsonValue;
    }
  | {
      type: "completed";
      result: OpencodeWorkerRunResult;
    }
  | {
      type: "error";
      message: string;
      error: JsonValue;
    };

export interface OpencodeWorkerRunResult {
  id: string;
  status: "finished" | "error";
  result?: string;
  model?: JsonValue;
  durationMs?: number;
}

export interface OpencodeModelRef {
  providerID: string;
  modelID: string;
}

/**
 * exo binds models by a single name (plus optional base URL and key). opencode
 * is provider-aware, so accept either a `provider/model` reference or a bare
 * model name that falls back to a configurable default provider.
 */
export function opencodeModelRef(
  model: string,
  defaultProvider?: string,
): OpencodeModelRef {
  const slash = model.indexOf("/");
  if (slash > 0) {
    return {
      providerID: model.slice(0, slash),
      modelID: model.slice(slash + 1),
    };
  }
  const provider =
    defaultProvider && defaultProvider.trim() ? defaultProvider : "anthropic";
  return { providerID: provider, modelID: model };
}

export function toOpencodeJson(value: unknown): JsonValue {
  if (value === undefined) {
    return null;
  }
  return JSON.parse(JSON.stringify(value)) as JsonValue;
}
