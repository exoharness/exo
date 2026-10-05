import assert from "node:assert/strict";

const base = process.env.EXO_WORKER_URL;
assert(base, "EXO_WORKER_URL is required (the Worker origin, without /exo)");
assert(process.env.EXO_TOKEN, "EXO_TOKEN is required");
assert(process.env.PROBE_KEY, "PROBE_KEY is required");
assert(process.env.OPENAI_API_KEY, "OPENAI_API_KEY is required");

async function api(path, method = "GET", body, expected = 200) {
  const response = await fetch(`${base}/exo/${path}`, {
    method,
    headers: {
      authorization: `Bearer ${process.env.EXO_TOKEN}`,
      "content-type": "application/json",
    },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    signal: AbortSignal.timeout(180_000),
  });
  const responseText = await response.text();
  assert(
    response.headers.get("content-type")?.includes("application/json"),
    `${method} ${path}: HTTP ${response.status}: ${responseText.slice(0, 200)}`,
  );
  const value = JSON.parse(responseText);
  assert.equal(response.status, expected, JSON.stringify(value));
  return value;
}
async function exec(path, source) {
  const result = await api(`${path}/sandbox/exec`, "POST", {
    command: ["node", "--input-type=module", "-e", source],
    timeoutMs: 30_000,
  });
  assert.equal(result.exitCode, 0, result.stderr);
  return result.stdout.trim();
}
const policy = (origin) => ({
  networking: {
    type: "destinations",
    allowed_destinations: [{ type: "origin", origin }],
  },
  injection_location: { header: true },
});
const existing = await api("vault");
const vault =
  existing.find((vault) => vault.name === "global") ??
  (await api("vault", "POST", { name: "global" }));
async function put(name, value, origin) {
  const existing = (await api(`vault/${vault.id}/secret`)).find(
    (secret) => secret.name === name,
  );
  const body = { secret: { type: "key", value }, policy: policy(origin) };
  if (existing) {
    await api(`vault/${vault.id}/secret/${existing.id}`, "PUT", body);
    return existing.id;
  }
  return api(`vault/${vault.id}/secret`, "POST", { name, ...body });
}
await put(
  "OPENAI_API_KEY",
  process.env.OPENAI_API_KEY,
  "https://api.openai.com",
);
const probeId = await put(
  "LIVE_PROBE",
  process.env.PROBE_KEY,
  new URL(base).origin,
);
const source = `---\nname: Cloudflare Live Test\nharness: basic\nconfig:\n  model: gpt-5-mini\n  credential: OPENAI_API_KEY\n  max_tool_round_trips: 3\n  max_output_tokens: 1024\n---\nUse the shell tool to read /workspace/persisted.txt. Reply with the exact contents. Do not claim you read a file without using shell.`;
const agent = await api("agent", "POST", {
  slug: `cloudflare-live-${Date.now()}`,
  name: "Cloudflare Live Test",
});
await api(`agent/${agent.id}/artifact`, "POST", {
  path: "managed-agents/agent.md",
  contents: [...new TextEncoder().encode(source)],
});
const { thread } = await api(`agent/${agent.id}/thread`, "POST", {});
const path = `agent/${agent.id}/thread/${thread.id}`;
console.log(`Testing ${path}`);
const checks = [];
try {
  await api(`${path}/sandbox/policy`, "PUT", {
    origins: [new URL(base).origin],
    credentials: [
      { environment_variable: "TEST_API_KEY", credential: "LIVE_PROBE" },
    ],
  });
  assert.equal(
    await exec(
      path,
      'import fs from "node:fs"; fs.writeFileSync("/workspace/persisted.txt", "exo-cloudflare-persistence-ok\\n"); console.log(process.platform, process.env.TEST_API_KEY.startsWith("exo_egress_"));',
    ),
    "linux true",
  );
  checks.push(
    "native Linux exec; only an opaque credential placeholder enters the sandbox",
  );
  const probe = `const r = await fetch(${JSON.stringify(`${base}/_probe`)}, {headers:{authorization:"Bearer "+process.env.TEST_API_KEY}, signal:AbortSignal.timeout(10000)}); console.log(r.status, await r.text());`;
  const result = await exec(path, probe);
  assert(result.startsWith("200 "), result);
  assert.equal(JSON.parse(result.slice(4)).authenticated, true);
  assert.equal(JSON.parse(result.slice(4)).placeholderReceived, false);
  checks.push("HTTPS interception and vault credential substitution");
  const basic = await exec(
    path,
    `const r = await fetch(${JSON.stringify(`${base}/_probe`)}, {headers:{authorization:"Basic "+btoa("user:"+process.env.TEST_API_KEY)}, signal:AbortSignal.timeout(10000)}); console.log(r.status, await r.text());`,
  );
  assert.equal(JSON.parse(basic.slice(4)).authenticated, true);
  checks.push("HTTP Basic credential substitution");
  assert.equal(
    await exec(
      path,
      'const r = await fetch("https://example.com", {signal:AbortSignal.timeout(10000)}); console.log(r.status);',
    ),
    "403",
  );
  checks.push("unlisted HTTPS destinations denied");
  assert.equal(
    await exec(
      path,
      `const r = await fetch(${JSON.stringify(`${base}/_probe`)}, {headers:{authorization:"Bearer exo_egress_unknown"}, signal:AbortSignal.timeout(10000)}); console.log(r.status);`,
    ),
    "403",
  );
  checks.push("unknown placeholders denied");
  assert.equal(
    await exec(
      path,
      'import tls from "node:tls"; const s = tls.connect({host:"1.1.1.1",port:443,rejectUnauthorized:false}); console.log(await new Promise(resolve => { const timer=setTimeout(()=>{s.destroy();resolve("blocked");},2000); s.on("secureConnect",()=>{clearTimeout(timer);s.destroy();resolve("connected");}); s.on("error",()=>{clearTimeout(timer);resolve("blocked");}); s.on("close",()=>{clearTimeout(timer);resolve("blocked");}); }));',
    ),
    "blocked",
  );
  checks.push(
    "direct IP TLS bypass denied even with certificate validation disabled",
  );
  assert.equal(
    await exec(
      path,
      'import dgram from "node:dgram"; const s=dgram.createSocket("udp4"); console.log(await new Promise(resolve=>{const timer=setTimeout(()=>{s.close();resolve("blocked");},2000); s.on("message",msg=>{clearTimeout(timer);s.close();resolve(JSON.stringify({hex:msg.toString("hex"),answers:msg.readUInt16BE(6)}));}); s.on("error",()=>{clearTimeout(timer);s.close();resolve("blocked");}); s.send(Buffer.from("123401000001000000000000076578616d706c6503636f6d0000100001","hex"),53,"1.1.1.1");}));',
    ),
    "blocked",
  );
  checks.push("direct UDP DNS bypass denied");
  const snapshot = await api(`${path}/sandbox/snapshot`, "POST", {});
  assert(snapshot.id);
  await api(`${path}/sandbox/stop`, "POST", {});
  assert.equal(
    await exec(
      path,
      'import fs from "node:fs"; console.log(fs.readFileSync("/workspace/persisted.txt", "utf8").trim());',
    ),
    "exo-cloudflare-persistence-ok",
  );
  assert((await exec(path, probe)).startsWith("200 "));
  checks.push(
    "filesystem snapshot restored after destruction, with egress interception reinstalled",
  );
  const artifact = await api(`${path}/artifact`, "POST", {
    path: "proof.txt",
    contents: [...new TextEncoder().encode("R2 persistence")],
  });
  assert.equal(
    new TextDecoder().decode(
      Uint8Array.from(
        (await api(`${path}/artifact/read?artifact_id=${artifact.artifact_id}`))
          .contents,
      ),
    ),
    "R2 persistence",
  );
  checks.push("R2 artifact read/write");
  const submitted = await api(
    `${path}/turn`,
    "POST",
    {
      input: {
        role: "user",
        content: "Read persisted.txt using shell and report its contents.",
      },
    },
    202,
  );
  let events;
  for (let i = 0; i < 180; i++) {
    events = (
      await api(`${path}/event?turn_id=${submitted.turn.id}&limit=1000`)
    ).events;
    if (events.some((event) => event.data.type === "turn_ended")) break;
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  assert(
    events.some((event) => event.data.type === "turn_ended"),
    "turn did not finish",
  );
  assert(
    !events.some((event) => event.data.type === "error"),
    JSON.stringify(events.filter((event) => event.data.type === "error")),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "tool_result" &&
        JSON.stringify(event.data.result).includes(
          "exo-cloudflare-persistence-ok",
        ),
    ),
  );
  assert(
    events.some(
      (event) =>
        event.data.type === "messages" &&
        JSON.stringify(event.data.messages).includes(
          "exo-cloudflare-persistence-ok",
        ),
    ),
  );
  checks.push(
    "real model → shell tool → model turn with durable canonical history",
  );
  await api(`vault/${vault.id}/secret/${probeId}`, "DELETE");
  assert.equal(await exec(path, probe), "403 Exo egress denied");
  checks.push("vault revocation applies to an already running sandbox");
  for (const check of checks) console.log(`PASS ${check}`);
} finally {
  await api(`${path}/sandbox/stop`, "POST", {});
}
