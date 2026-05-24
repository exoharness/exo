export type WorkerOutboundCommand = {
  type: "send_message";
  target: string;
  text: string;
};

export type WorkerInboundEvent =
  | {
      type: "qr";
      qr: string;
    }
  | {
      type: "connected";
      jid: string | null;
    }
  | {
      type: "message";
      chat_id: string;
      sender: string | null;
      text: string;
      message_id: string | null;
    }
  | {
      type: "error";
      message: string;
    }
  | {
      type: "disconnected";
      reason: string | null;
    };

export function parseWorkerCommand(value: unknown): WorkerOutboundCommand {
  if (!isRecord(value) || value.type !== "send_message") {
    throw new Error("worker command must be a send_message object");
  }
  if (typeof value.target !== "string" || value.target.length === 0) {
    throw new Error("send_message target must be a non-empty string");
  }
  if (typeof value.text !== "string" || value.text.length === 0) {
    throw new Error("send_message text must be a non-empty string");
  }
  return {
    type: "send_message",
    target: value.target,
    text: value.text,
  };
}

export function writeWorkerEvent(event: WorkerInboundEvent): void {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === "object" && !Array.isArray(value);
}
