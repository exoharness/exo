import type { SandboxProcess } from "../../harness";

interface Request {
  id?: number;
  method: string;
  params?: Record<string, unknown>;
}

export class FakeCodexAppServer {
  readonly requests: Request[] = [];
  readonly process: SandboxProcess;
  private stdout!: ReadableStreamDefaultController<string>;
  private turns = new Map<string, Record<string, unknown>[]>();
  private nextTurn = 1;

  constructor(
    private readonly options: {
      resumeAvailable: boolean;
      reused?: boolean;
      oldTurn?: Record<string, unknown>;
    },
  ) {
    this.turns.set("old-thread", [
      options.oldTurn ?? {
        id: "old-turn",
        status: "interrupted",
        itemsView: "full",
        items: [],
        error: null,
      },
    ]);
    const stdout = new ReadableStream<string>({
      start: (controller) => {
        this.stdout = controller;
      },
    });
    const stderr = new ReadableStream<string>({
      start: (controller) => controller.close(),
    });
    this.process = {
      reused: options.reused ?? false,
      sandboxId: "sandbox-1",
      sandboxProcessId: "process-1",
      stdout,
      stderr,
      writeStdin: async (data) => {
        const request = JSON.parse(data) as Request;
        this.requests.push(request);
        this.respond(request);
      },
      closeStdin: async () => {},
      close: async () => {
        this.stdout.close();
      },
      wait: () => new Promise<number | null>(() => {}),
    };
  }

  private emit(value: unknown): void {
    this.stdout.enqueue(`${JSON.stringify(value)}\n`);
  }

  private respond(request: Request): void {
    if (request.id === undefined) return;
    const params = request.params ?? {};
    const threadId = String(params.threadId);
    switch (request.method) {
      case "initialize":
      case "thread/inject_items":
        this.emit({ id: request.id, result: {} });
        return;
      case "thread/resume":
        if (!this.options.resumeAvailable) {
          this.emit({
            id: request.id,
            error: { message: "thread unavailable" },
          });
          return;
        }
        this.emit({ id: request.id, result: { thread: { id: threadId } } });
        return;
      case "thread/start":
        this.turns.set("new-thread", []);
        this.emit({ id: request.id, result: { thread: { id: "new-thread" } } });
        return;
      case "thread/read":
        this.emit({
          id: request.id,
          result: {
            thread: { id: threadId, turns: this.turns.get(threadId) ?? [] },
          },
        });
        return;
      case "turn/start": {
        const id = `new-turn-${this.nextTurn++}`;
        const turn = {
          id,
          status: "completed",
          itemsView: "full",
          items: [{ id: `answer-${id}`, type: "agentMessage", text: "done" }],
          error: null,
        };
        this.turns.get(threadId)?.push(turn);
        this.emit({ id: request.id, result: { turn: { id } } });
        this.emit({ method: "turn/completed", params: { threadId, turn } });
        return;
      }
      default:
        this.emit({
          id: request.id,
          error: { message: `unexpected ${request.method}` },
        });
    }
  }
}
