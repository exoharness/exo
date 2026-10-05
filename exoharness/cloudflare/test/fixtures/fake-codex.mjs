// A JSONL app-server fixture exercising actual Worker RPC byte streams.
export const fakeCodex = `
class FakeCodexProcess extends RpcTarget {
  constructor(storage, history) {
    super();
    this.storage = storage; this.history = history; this.turns = [];
    this.output = new ReadableStream({type: "bytes", start: controller => { this.controller = controller; }});
    this.errors = new ReadableStream({type: "bytes", start: controller => controller.close()});
    this.exited = new Promise(resolve => { this.exit = resolve; });
  }
  get stdout() { return this.output; }
  get stderr() { return this.errors; }
  get sandboxProcessId() { return "fixture-process"; }
  emit(value) { this.controller.enqueue(new TextEncoder().encode(JSON.stringify(value) + "\\n")); }
  async writeStdin(line) {
    const request = JSON.parse(line), p = request.params ?? {};
    if (request.id === undefined) return;
    if (request.method === "initialize") this.emit({id:request.id, result:{}});
    else if (request.method === "thread/start" || request.method === "thread/resume") {
      await this.storage.put("last-method", request.method);
      this.rawEvents = request.method === "thread/start" && p.experimentalRawEvents;
      this.emit({id:request.id, result:{thread:{id:p.threadId ?? "fixture-thread"}}});
      if (request.method === "thread/resume") {
        this.emit({method:"thread/tokenUsage/updated", params:{threadId:p.threadId, turnId:"previous-turn", tokenUsage:{last:{inputTokens:5, outputTokens:2, totalTokens:7}, total:{totalTokens:await this.storage.get("usage-total")}}}});
      }
    } else if (request.method === "thread/read") this.emit({id:request.id, result:{thread:{id:"fixture-thread", turns:this.turns}}});
    else if (request.method === "turn/start") {
      const id = "turn-" + crypto.randomUUID(), threadId = p.threadId;
      const turn = {id, status:"completed", itemsView:"full", error:null, items:[
        {id:"cmd-"+id, type:"commandExecution", command:"node --test", cwd:"/workspace", status:"completed", exitCode:0, aggregatedOutput:"passed", durationMs:5},
        {id:"msg-"+id, type:"agentMessage", text:"Done."},
      ]};
      this.turns.push(turn);
      this.emit({id:request.id, result:{turn:{id}}});
      this.emit({method:"item/agentMessage/delta", params:{threadId, turnId:id, itemId:"msg-"+id, delta:"Done."}});
      let total = await this.storage.get("usage-total") ?? 0;
      for (const last of [{inputTokens:10, outputTokens:3, totalTokens:13, cachedInputTokens:4}, {inputTokens:5, outputTokens:2, totalTokens:7, cachedInputTokens:1}]) {
        total += last.totalTokens;
        const params = {threadId, turnId:id, tokenUsage:{last, total:{totalTokens:total}}};
        if (this.rawEvents) this.emit({method:"rawResponse/completed", params:{threadId, turnId:id, usage:last}});
        this.emit({method:"thread/tokenUsage/updated", params});
        this.emit({method:"thread/tokenUsage/updated", params});
      }
      await this.storage.put("usage-total", total);
      this.emit({method:"turn/completed", params:{threadId, turn}});
    } else this.emit({id:request.id, error:{message:"Unexpected method " + request.method}});
  }
  async closeStdin() {}
  async close() { this.controller.close(); this.exit(0); }
  async wait() { return this.exited; }
}
`;
