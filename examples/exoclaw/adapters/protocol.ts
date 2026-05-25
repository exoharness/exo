export type WorkerOutboundCommand = {
  type: "send_message";
  target?: string | null;
  text: string;
};

export type WorkerInboundEvent =
  | {
      type: "connected";
      subject?: string | null;
      metadata?: JsonObject;
    }
  | {
      type: "message";
      target: string;
      sender?: string | null;
      text: string;
      message_id?: string | null;
      metadata?: JsonObject;
    }
  | {
      type: "lifecycle";
      name: string;
      metadata?: JsonObject;
    }
  | {
      type: "error";
      message: string;
    }
  | {
      type: "disconnected";
      reason?: string | null;
    };

type JsonObject = Record<string, unknown>;
let stdoutErrorHandlerInstalled = false;

export function adapterConfig(): JsonObject {
  const raw = process.env.EXO_ADAPTER_CONFIG;
  if (!raw) {
    return {};
  }
  const parsed = JSON.parse(raw) as unknown;
  if (!isRecord(parsed)) {
    throw new Error("EXO_ADAPTER_CONFIG must contain a JSON object");
  }
  return parsed;
}

export function parseWorkerCommand(value: unknown): WorkerOutboundCommand {
  if (!isRecord(value) || value.type !== "send_message") {
    throw new Error("worker command must be a send_message object");
  }
  if (
    value.target !== undefined &&
    value.target !== null &&
    (typeof value.target !== "string" || value.target.length === 0)
  ) {
    throw new Error("send_message target must be null or a non-empty string");
  }
  if (typeof value.text !== "string" || value.text.length === 0) {
    throw new Error("send_message text must be a non-empty string");
  }
  return {
    type: "send_message",
    target: value.target ?? null,
    text: value.text,
  };
}

export function stringField(config: JsonObject, name: string): string {
  const value = config[name];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`adapter config ${name} must be a non-empty string`);
  }
  return value;
}

export function optionalStringField(
  config: JsonObject,
  name: string,
): string | null {
  const value = config[name];
  if (value === undefined || value === null) {
    return null;
  }
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(
      `adapter config ${name} must be null or a non-empty string`,
    );
  }
  return value;
}

export function booleanField(config: JsonObject, name: string): boolean {
  const value = config[name];
  if (typeof value !== "boolean") {
    throw new Error(`adapter config ${name} must be a boolean`);
  }
  return value;
}

export function numberField(config: JsonObject, name: string): number {
  const value = config[name];
  if (typeof value !== "number") {
    throw new Error(`adapter config ${name} must be a number`);
  }
  return value;
}

export function writeWorkerEvent(event: WorkerInboundEvent): void {
  ensureStdoutErrorHandler();
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

export function isRecord(value: unknown): value is JsonObject {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}

function ensureStdoutErrorHandler(): void {
  if (stdoutErrorHandlerInstalled) {
    return;
  }
  stdoutErrorHandlerInstalled = true;
  process.stdout.on("error", (error: NodeJS.ErrnoException) => {
    if (error.code === "EPIPE") {
      process.exit(0);
    }
    throw error;
  });
}
