import { DurableObject, RpcTarget, RpcStub } from "cloudflare:workers";
import { FakeCodexAppServer } from "./fake-app-server.js";
class FakeProcess extends RpcTarget {
  constructor(process, suppress = () => false) {
    super();
    this.process = process;
    this.out = process.stdout
      .pipeThrough(
        new TransformStream({
          transform(line, controller) {
            if (!suppress(line)) controller.enqueue(line);
          },
        }),
      )
      .pipeThrough(new TextEncoderStream())
      .pipeThrough(
        new TransformStream({
          transform(bytes, controller) {
            const index = bytes.findIndex((byte) => byte >= 128);
            if (index < 0) controller.enqueue(bytes);
            else {
              controller.enqueue(bytes.slice(0, index + 1));
              controller.enqueue(bytes.slice(index + 1));
            }
          },
        }),
      );
    this.err = process.stderr.pipeThrough(new TextEncoderStream());
  }
  get stdout() {
    return this.out;
  }
  get stderr() {
    return this.err;
  }
  async writeStdin(bytes) {
    await this.process.writeStdin(new TextDecoder().decode(bytes));
  }
  async closeStdin() {
    await this.process.closeStdin();
  }
  async close() {
    await this.process.close();
  }
  async wait() {
    return this.process.wait();
  }
}
class EchoProcess extends RpcTarget {
  constructor() {
    super();
    this.output = new ReadableStream({
      type: "bytes",
      start: (c) => {
        this.controller = c;
      },
    });
    this.exited = new Promise((resolve) => {
      this.exit = resolve;
    });
    this.closed = false;
  }
  get stdout() {
    return this.output;
  }
  get stderr() {
    return new ReadableStream({ type: "bytes", start: (c) => c.close() });
  }
  async writeStdin(bytes) {
    this.controller.enqueue(bytes);
  }
  async closeStdin() {
    if (!this.closed) {
      this.closed = true;
      this.controller.close();
      this.exit(0);
    }
  }
  async close() {
    await this.closeStdin();
  }
  async wait() {
    return this.exited;
  }
}
export class FakeSandbox extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.processes = new Set();
    this.activities = new Set();
  }
  async beginActivity(identity, id) {
    this.activities.add(id);
    await this.ctx.storage.put("identity", identity);
    await this.ctx.storage.deleteAlarm();
  }
  async endActivity(id) {
    this.activities.delete(id);
  }
  async activityCount() {
    return this.activities.size;
  }
  async info() {
    return {
      exists: !!(await this.ctx.storage.get("identity")),
      running: (await this.ctx.storage.get("running")) ?? false,
    };
  }
  async acquire(
    identity,
    image,
    cwd,
    environment,
    snapshot,
    idleMs,
    resources,
  ) {
    await this.ctx.storage.put("resources", resources ?? null);
    image ||= "cloudflare/debian-trixie";
    await this.ctx.storage.put("identity", identity);
    await this.ctx.storage.put("image", image);
    await this.ctx.storage.put("environment", environment);
    await this.ctx.storage.put("running", true);
    return image;
  }
  async terminate() {
    await this.stop();
    await this.ctx.storage.deleteAll();
  }
  async exec(_identity, _request) {
    const count = ((await this.ctx.storage.get("count")) ?? 0) + 1;
    await this.ctx.storage.put("count", count);
    return { stdout: "Linux test\n", stderr: "", exitCode: 0 };
  }
  async count() {
    return (await this.ctx.storage.get("count")) ?? 0;
  }
  async environment() {
    return await this.ctx.storage.get("environment");
  }
  async resources() {
    return JSON.stringify(await this.ctx.storage.get("resources"));
  }
  async startProcess(request) {
    if (request.command[0] === "cat") return new EchoProcess();
    if (!request.command.join(" ").includes("codex")) {
      if (request.command[2] !== "true")
        await this.ctx.storage.put("count", (await this.count()) + 1);
      const git = request.command.join(" ").includes("--no-recurse-submodules");
      const failed = request.command
        .join(" ")
        .includes("example/disk-full.git");
      return new FakeProcess({
        stdout: new ReadableStream({
          start(c) {
            c.enqueue("Linux test\n");
            c.close();
          },
        }),
        stderr: new ReadableStream({
          start(c) {
            if (git)
              c.enqueue(
                failed
                  ? "fatal: write error: No space left on device\n"
                  : "Receiving objects:  50% (1/2)\rReceiving objects: 100% (2/2)\r",
              );
            c.close();
          },
        }),
        writeStdin: async () => {},
        closeStdin: async () => {},
        close: async () => {},
        wait: async () => (failed ? 128 : 0),
      });
    }
    let holdTurn = false;
    const app = new FakeCodexAppServer({
      resumeAvailable: true,
      completedItems: (id) => [
        {
          id: "cmd-" + id,
          type: "commandExecution",
          command: "node --test",
          cwd: "/workspace",
          status: "completed",
          exitCode: 0,
          aggregatedOutput: "passed",
          durationMs: 5,
        },
        { id: "msg-" + id, type: "agentMessage", text: "Done. 🧪" },
      ],
      onTurn: async (threadId, turnId, emit) => {
        emit({
          method: "item/agentMessage/delta",
          params: {
            threadId,
            turnId,
            itemId: "msg-" + turnId,
            delta: "Done. 🧪",
          },
        });
      },
    });
    const write = app.process.writeStdin;
    app.process.writeStdin = async (line) => {
      const request = JSON.parse(line);
      if (request.method === "turn/start")
        holdTurn = JSON.stringify(request.params).includes("hold-turn");
      if (["thread/start", "thread/resume"].includes(request.method)) {
        await this.ctx.storage.put("last-method", request.method);
        await this.ctx.storage.put(
          "mcp-config",
          request.params.config.mcp_servers,
        );
      }
      await write(line);
    };
    return new FakeProcess(
      app.process,
      (line) => holdTurn && JSON.parse(line).method === "turn/completed",
    );
  }
  blockNextStart() {
    this.blockStart = true;
    this.blocked = new Promise((resolve) => (this.notifyBlocked = resolve));
  }
  waitForBlockedStart() {
    return this.blocked;
  }
  releaseBlockedStart() {
    this.releaseStart();
  }
  async runProcess(identity, request, ready) {
    const process = await this.startProcess(request);
    this.processes.add(process);
    if (this.blockStart && request.command.join(" ").includes("codex")) {
      this.blockStart = false;
      await new Promise((resolve) => {
        this.releaseStart = resolve;
        this.notifyBlocked();
      });
    }
    const capability = new RpcStub(process);
    try {
      await ready(capability);
      await process.wait();
    } finally {
      capability[Symbol.dispose]();
      this.processes.delete(process);
    }
  }

  async snapshot() {
    return { id: "fixture-snapshot", size: 1 };
  }
  async processCount() {
    return this.processes.size;
  }
  async lastMethod() {
    return await this.ctx.storage.get("last-method");
  }
  async mcpConfig() {
    return await this.ctx.storage.get("mcp-config");
  }
  async stop() {
    await Promise.all([...this.processes].map((p) => p.close()));
    await this.ctx.storage.put("running", false);
  }
}
export default {
  fetch() {
    return new Response("fixture");
  },
};
