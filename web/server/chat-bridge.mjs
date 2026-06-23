// Executor adapter seam: the substrate HTTP transport does not run agent turns or
// create conversations, so this local bridge invokes the exo CLI (`conversation
// send` and `conversation create`) as the executor entry point. Keep it thin — no
// streaming, no substrate reads beyond what the CLI exposes. A future native
// executor HTTP API can replace this module without touching the web UI.
import { spawn } from "node:child_process";
import { createServer } from "node:http";

// - EXO_BIN: path to the exo CLI (default: "exo" on PATH)
// - EXO_HARNESS: harness flag value (default: "exoclaw")
// - EXO_CWD: working directory where exo finds state/env
// - CHAT_BRIDGE_PORT: localhost HTTP port Vite proxies /chat to
// - CHAT_TURN_TIMEOUT_MS: per-turn timeout; child is killed on expiry
const EXO_BIN = process.env.EXO_BIN || "exo";
const EXO_HARNESS = process.env.EXO_HARNESS || "exoclaw";
const EXO_CWD = process.env.EXO_CWD || process.cwd();
const PORT = readPositiveInt(process.env.CHAT_BRIDGE_PORT, 4767);
const TURN_TIMEOUT_MS = readPositiveInt(
  process.env.CHAT_TURN_TIMEOUT_MS,
  300_000,
);

const MAX_BODY_CHARS = 64 * 1024;
const MAX_CAPTURE_CHARS = 32 * 1024;
const STDERR_PREVIEW_CHARS = 2000;
const inFlightTurns = new Map();

const server = createServer(async (request, response) => {
  const url = new URL(request.url || "/", "http://127.0.0.1");

  if (request.method === "GET" && url.pathname === "/health") {
    sendText(response, 200, "ok");
    return;
  }

  if (request.method === "POST" && isCancelPath(url.pathname)) {
    await handleCancel(request, response);
    return;
  }

  if (
    request.method === "POST" &&
    (url.pathname === "/create" || url.pathname === "/chat/create")
  ) {
    await handleCreate(request, response);
    return;
  }

  if (request.method !== "POST" || url.pathname !== "/chat") {
    sendJson(response, 404, {
      ok: false,
      requestId: null,
      exitCode: null,
      error: "not found",
    });
    return;
  }

  let payload;
  try {
    payload = await readJsonBody(request);
  } catch (error) {
    sendJson(response, 400, {
      ok: false,
      requestId: null,
      exitCode: null,
      error: errorMessage(error),
    });
    return;
  }

  const validationError = validateChatPayload(payload);
  if (validationError) {
    sendJson(response, 400, {
      ok: false,
      requestId: readRequestId(payload),
      exitCode: null,
      error: validationError,
    });
    return;
  }

  const result = await runTurn(payload);
  sendJson(response, statusCodeForChatResult(result), result);
});

server.on("error", (error) => {
  console.error(
    `chat bridge failed to listen on 127.0.0.1:${PORT}: ${error.message}`,
  );
  process.exitCode = 1;
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`chat bridge listening on http://127.0.0.1:${PORT}`);
});

async function runTurn({ agent, conversation, message, requestId }) {
  const args = [
    "--harness",
    EXO_HARNESS,
    "conversation",
    "send",
    // End-of-options separator: without it a message starting with "-" (e.g.
    // "--help") is parsed by the exo CLI as a flag and the turn fails. After "--"
    // the agent, conversation, and message are all forced to positional args.
    "--",
    agent,
    conversation,
    message,
  ];

  if (inFlightTurns.has(requestId)) {
    return {
      ok: false,
      requestId,
      exitCode: null,
      error: `chat request ${requestId} is already in flight`,
    };
  }

  const childResult = await spawnAndWait(EXO_BIN, args, TURN_TIMEOUT_MS, {
    requestId,
    trackTurn: true,
  });
  if (!childResult.ok) {
    return {
      ok: false,
      requestId,
      exitCode: childResult.exitCode,
      error: childResult.error,
      stderr: childResult.stderr,
    };
  }

  const latestEventId = await readLatestEventId(agent, conversation);

  return {
    ok: true,
    requestId,
    exitCode: childResult.exitCode,
    error: null,
    stderr: null,
    latestEventId,
  };
}

async function handleCreate(request, response) {
  let payload;
  try {
    payload = await readJsonBody(request);
  } catch (error) {
    sendJson(response, 400, { ok: false, error: errorMessage(error) });
    return;
  }

  const agent =
    payload && typeof payload.agent === "string" ? payload.agent.trim() : "";
  if (!agent) {
    sendJson(response, 400, {
      ok: false,
      error: "agent must be a non-empty string",
    });
    return;
  }
  const name =
    payload && typeof payload.name === "string" ? payload.name.trim() : "";

  // `--` forces the agent (and optional name) to be read as positionals even if a
  // name begins with "-". Mirrors the send path's argument handling.
  const args = [
    "--harness",
    EXO_HARNESS,
    "conversation",
    "create",
    "--",
    agent,
  ];
  if (name) {
    args.push(name);
  }

  const result = await spawnAndWait(EXO_BIN, args, 30_000);
  if (!result.ok) {
    sendJson(response, 500, {
      ok: false,
      exitCode: result.exitCode,
      error: result.error,
      stderr: result.stderr,
    });
    return;
  }

  // The CLI prints: "created conversation <slug> (<id>)".
  const match = result.stdout.match(
    /created conversation\s+(\S+)\s+\(([0-9a-f-]+)\)/i,
  );
  sendJson(response, 200, {
    ok: true,
    slug: match?.[1] ?? null,
    id: match?.[2] ?? null,
  });
}

async function handleCancel(request, response) {
  let payload;
  try {
    payload = await readJsonBody(request);
  } catch (error) {
    sendJson(response, 400, {
      ok: false,
      requestId: null,
      exitCode: null,
      error: errorMessage(error),
    });
    return;
  }

  const requestId = readRequestId(payload);
  if (!requestId) {
    sendJson(response, 400, {
      ok: false,
      requestId,
      exitCode: null,
      error: "requestId must be a non-empty string",
    });
    return;
  }

  const entry = inFlightTurns.get(requestId);
  if (!entry) {
    sendJson(response, 404, {
      ok: false,
      requestId,
      exitCode: null,
      error: `no in-flight chat turn for request ${requestId}`,
    });
    return;
  }

  entry.cancelled = true;
  const signalled = entry.child.kill("SIGTERM");
  sendJson(response, signalled ? 200 : 409, {
    ok: signalled,
    requestId,
    exitCode: null,
    error: signalled ? null : `failed to signal chat request ${requestId}`,
  });
}

function spawnAndWait(command, args, timeoutMs, options = {}) {
  return new Promise((resolve) => {
    const child = spawn(command, args, {
      cwd: EXO_CWD,
      shell: false,
    });

    let stdout = "";
    let stderr = "";
    let timedOut = false;
    let settled = false;
    const turnEntry = options.trackTurn ? { child, cancelled: false } : null;

    if (turnEntry) {
      inFlightTurns.set(options.requestId, turnEntry);
    }

    const timeout = setTimeout(() => {
      timedOut = true;
      child.kill("SIGTERM");
      finish({
        ok: false,
        exitCode: null,
        error: `exo conversation send timed out after ${timeoutMs}ms`,
        timedOut: true,
        stderr: trimPreview(stderr),
      });
    }, timeoutMs);

    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");

    child.stdout?.on("data", (chunk) => {
      stdout = appendCaptured(stdout, chunk);
    });
    child.stderr?.on("data", (chunk) => {
      stderr = appendCaptured(stderr, chunk);
    });

    child.on("error", (error) => {
      finish({
        ok: false,
        exitCode: null,
        error: `failed to start ${command}: ${error.message}`,
        stderr: trimPreview(stderr),
      });
    });

    child.on("close", (code, signal) => {
      if (timedOut) {
        return;
      }

      if (code === 0) {
        finish({
          ok: true,
          exitCode: 0,
          stdout,
          stderr: trimPreview(stderr),
        });
        return;
      }

      if (turnEntry?.cancelled) {
        finish({
          ok: false,
          exitCode: code,
          cancelled: true,
          error: "chat turn cancelled",
          stderr: trimPreview(stderr),
        });
        return;
      }

      finish({
        ok: false,
        exitCode: code,
        error: formatChildFailure(code, signal, stdout, stderr),
        stderr: trimPreview(stderr),
      });
    });

    function finish(result) {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timeout);
      if (options.trackTurn) {
        inFlightTurns.delete(options.requestId);
      }
      resolve(result);
    }
  });
}

async function readLatestEventId(agent, conversation) {
  const args = [
    "--harness",
    EXO_HARNESS,
    "conversation",
    "show",
    agent,
    conversation,
  ];
  const result = await spawnAndWait(EXO_BIN, args, 15_000);
  if (!result.ok) {
    return null;
  }

  const match = result.stdout.match(/latest_event_id:\s*(\S+)/);
  const eventId = match?.[1] ?? null;
  return eventId && eventId !== "none" ? eventId : null;
}

function validateChatPayload(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return "body must be a JSON object";
  }

  for (const key of ["agent", "conversation", "message"]) {
    if (typeof value[key] !== "string" || value[key].trim() === "") {
      return `${key} must be a non-empty string`;
    }
  }

  if (!readRequestId(value)) {
    return "requestId must be a non-empty string";
  }

  return null;
}

function readRequestId(value) {
  return value &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    typeof value.requestId === "string" &&
    value.requestId.trim() !== ""
    ? value.requestId.trim()
    : null;
}

function isCancelPath(pathname) {
  return pathname === "/cancel" || pathname === "/chat/cancel";
}

function statusCodeForChatResult(result) {
  if (result.ok) {
    return 200;
  }
  if (result.cancelled) {
    return 499;
  }
  if (result.timedOut) {
    return 504;
  }
  return 500;
}

function readJsonBody(request) {
  return new Promise((resolve, reject) => {
    let body = "";
    let tooLarge = false;

    request.setEncoding("utf8");
    request.on("data", (chunk) => {
      body += chunk;
      if (body.length > MAX_BODY_CHARS) {
        tooLarge = true;
        reject(new Error("request body is too large"));
        request.destroy();
      }
    });
    request.on("end", () => {
      if (tooLarge) {
        return;
      }
      try {
        resolve(JSON.parse(body));
      } catch {
        reject(new Error("body must be valid JSON"));
      }
    });
    request.on("error", reject);
  });
}

function sendJson(response, statusCode, payload) {
  response.writeHead(statusCode, {
    "content-type": "application/json; charset=utf-8",
    "cache-control": "no-store",
  });
  response.end(JSON.stringify(payload));
}

function sendText(response, statusCode, body) {
  response.writeHead(statusCode, {
    "content-type": "text/plain; charset=utf-8",
    "cache-control": "no-store",
  });
  response.end(body);
}

function appendCaptured(current, chunk) {
  const next = `${current}${chunk}`;
  return next.length > MAX_CAPTURE_CHARS
    ? next.slice(-MAX_CAPTURE_CHARS)
    : next;
}

function trimPreview(value) {
  const trimmed = value.trim();
  if (!trimmed) {
    return null;
  }
  return trimmed.length > STDERR_PREVIEW_CHARS
    ? trimmed.slice(-STDERR_PREVIEW_CHARS)
    : trimmed;
}

function formatChildFailure(code, signal, stdout, stderr) {
  const parts = [
    `exo conversation send exited with ${code == null ? "no code" : `code ${code}`}`,
  ];
  if (signal) {
    parts.push(`signal ${signal}`);
  }
  if (stderr.trim()) {
    parts.push(`stderr: ${stderr.trim()}`);
  } else if (stdout.trim()) {
    parts.push(`stdout: ${stdout.trim()}`);
  }
  return parts.join("; ");
}

function readPositiveInt(value, fallback) {
  if (!value) {
    return fallback;
  }
  const parsed = Number.parseInt(value, 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
}
