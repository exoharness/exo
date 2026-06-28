import "./chat.css";

(async () => {
  const session = readSession();
  const state = {
    channelId: session.channelId,
    role: session.role,
    secret: readSecret(),
    seq: 0,
    ws: null,
    pc: null,
    dc: null,
    connected: false,
    remoteSeen: false,
    failureTimer: null,
    offerStarted: false,
  };

  const els = {
    title: document.querySelector("#title"),
    subtitle: document.querySelector("#subtitle"),
    role: document.querySelector("#role"),
    signal: document.querySelector("#signal"),
    peer: document.querySelector("#peer"),
    data: document.querySelector("#data"),
    log: document.querySelector("#log"),
    scroll: document.querySelector("#scroll"),
    form: document.querySelector("#form"),
    input: document.querySelector("#input"),
    send: document.querySelector("#send"),
    hint: document.querySelector("#hint"),
  };

  if (!state.channelId || !state.secret) {
    addNotice(
      "Missing channel id or secret. Start from the URL printed by the demo script.",
    );
    return;
  }

  els.title.textContent =
    state.role === "agent"
      ? "exo chat - desktop peer"
      : "exo chat - phone peer";
  els.subtitle.textContent = `session ${state.channelId.slice(0, 8)}`;
  setPill(els.role, state.role, "good");
  setPill(els.signal, "signaling: connecting", "warn");
  setPill(els.peer, "peer: waiting", "warn");
  setPill(els.data, "datachannel: closed");
  setComposer(false, "Waiting for peer connection");

  const hmacKey = await crypto.subtle.importKey(
    "raw",
    base64urlToBytes(state.secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );

  state.pc = createPeerConnection();
  state.ws = connectSignaling();

  els.form.addEventListener("submit", (event) => {
    event.preventDefault();
    submitComposer();
  });

  els.input.addEventListener("input", () => {
    autosizeComposer();
  });

  els.input.addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.shiftKey) {
      return;
    }

    event.preventDefault();
    submitComposer();
  });

  function createPeerConnection() {
    const pc = new RTCPeerConnection({
      iceServers: [{ urls: "stun:stun.cloudflare.com:3478" }],
    });

    pc.addEventListener("icecandidate", (event) => {
      if (event.candidate) {
        sendSignal({
          type: "candidate",
          candidate: event.candidate.toJSON(),
        });
      }
    });

    pc.addEventListener("connectionstatechange", () => {
      const status = pc.connectionState;
      setPill(
        els.peer,
        `peer: ${status}`,
        status === "connected"
          ? "good"
          : status === "failed" || status === "disconnected"
            ? "bad"
            : "warn",
      );
    });

    if (state.role === "user") {
      attachDataChannel(pc.createDataChannel("exo", { ordered: true }));
    } else {
      pc.addEventListener("datachannel", (event) => {
        attachDataChannel(event.channel);
      });
    }

    return pc;
  }

  function connectSignaling() {
    const wsUrl = new URL(`/chat/ws/${state.channelId}`, location.href);
    wsUrl.protocol = location.protocol === "https:" ? "wss:" : "ws:";
    wsUrl.searchParams.set("role", state.role);

    const ws = new WebSocket(wsUrl);

    ws.addEventListener("open", async () => {
      setPill(els.signal, "signaling: open", "good");
      await sendSignal({
        type: "hello",
        role: state.role,
        nonce: bytesToBase64url(crypto.getRandomValues(new Uint8Array(16))),
      });
      addNotice(
        state.role === "user"
          ? "Waiting for the desktop peer..."
          : "Waiting for the phone peer...",
      );
    });

    ws.addEventListener("close", () => {
      setPill(els.signal, "signaling: closed", state.connected ? "" : "bad");
      setComposer(false, "Signaling closed");
    });

    ws.addEventListener("error", () => {
      setPill(els.signal, "signaling: error", "bad");
      setComposer(false, "Signaling error");
    });

    ws.addEventListener("message", async (event) => {
      const message = JSON.parse(event.data);
      if (message.channel === "rendezvous" && message.type === "presence") {
        await handlePresence(message.roles ?? []);
        return;
      }

      if (!(await verifySignedSignal(message))) {
        addNotice("Rejected a signaling message with an invalid signature.");
        return;
      }

      await handleSignal(message.body);
    });

    return ws;
  }

  async function handlePresence(roles) {
    const remoteRole = state.role === "agent" ? "user" : "agent";
    if (roles.includes(remoteRole) && !state.remoteSeen) {
      state.remoteSeen = true;
      setPill(els.peer, "peer: connecting", "warn");
      addNotice(
        state.role === "agent"
          ? "Phone peer joined. Starting direct connection..."
          : "Desktop peer joined. Starting direct connection...",
      );
      startFailureTimer();
    }

    await maybeStartOffer(roles);
  }

  function startFailureTimer() {
    if (state.failureTimer) {
      clearTimeout(state.failureTimer);
    }

    state.failureTimer = setTimeout(() => {
      if (!state.connected) {
        setPill(els.peer, "peer: failed", "bad");
        setComposer(false, "Direct connection failed");
        addNotice(
          "Could not establish a direct connection. Try switching Wi-Fi/cellular, disabling VPN or Private Relay, and keeping the desktop peer open.",
        );
      }
    }, 12000);
  }

  async function maybeStartOffer(roles) {
    if (
      state.role !== "user" ||
      state.offerStarted ||
      !roles.includes("agent") ||
      state.pc.signalingState !== "stable"
    ) {
      return;
    }

    state.offerStarted = true;
    const offer = await state.pc.createOffer();
    await state.pc.setLocalDescription(offer);
    await sendSignal({ type: "offer", sdp: offer.sdp });
  }

  async function handleSignal(body) {
    if (body.type === "hello") {
      return;
    }

    if (body.type === "offer" && state.role === "agent") {
      await state.pc.setRemoteDescription({ type: "offer", sdp: body.sdp });
      const answer = await state.pc.createAnswer();
      await state.pc.setLocalDescription(answer);
      await sendSignal({ type: "answer", sdp: answer.sdp });
      return;
    }

    if (body.type === "answer" && state.role === "user") {
      await state.pc.setRemoteDescription({ type: "answer", sdp: body.sdp });
      return;
    }

    if (body.type === "candidate" && body.candidate) {
      await state.pc.addIceCandidate(body.candidate);
    }
  }

  function attachDataChannel(channel) {
    state.dc = channel;

    channel.addEventListener("open", () => {
      state.connected = true;
      if (state.failureTimer) {
        clearTimeout(state.failureTimer);
        state.failureTimer = null;
      }
      setPill(els.peer, "peer: connected", "good");
      setPill(els.data, "datachannel: open", "good");
      setComposer(true, "Enter to send, Shift+Enter for newline");
      addNotice("Direct WebRTC DataChannel connected.");
    });

    channel.addEventListener("close", () => {
      setPill(els.data, "datachannel: closed", state.connected ? "" : "bad");
      setComposer(false, "Connection closed");
    });

    channel.addEventListener("message", (event) => {
      renderFrame(JSON.parse(event.data), false);
    });
  }

  function submitComposer() {
    const text = els.input.value.trim();
    if (!text || !state.dc || state.dc.readyState !== "open") {
      return;
    }

    sendChat(text);
    els.input.value = "";
    autosizeComposer();
  }

  function sendChat(text) {
    const frame = {
      type: "chat",
      id: crypto.randomUUID(),
      from: state.role,
      text,
      createdAt: Date.now(),
    };
    state.dc.send(JSON.stringify(frame));
    renderFrame(frame, true);
  }

  async function sendSignal(body) {
    const signal = {
      channelId: state.channelId,
      from: state.role,
      seq: ++state.seq,
      body,
    };
    const mac = await signSignal(signal);
    state.ws.send(JSON.stringify({ ...signal, mac }));
  }

  async function signSignal(signal) {
    const bytes = new TextEncoder().encode(canonicalSignal(signal));
    const signature = await crypto.subtle.sign("HMAC", hmacKey, bytes);
    return bytesToBase64url(new Uint8Array(signature));
  }

  async function verifySignedSignal(signal) {
    if (!signal || signal.channelId !== state.channelId || !signal.mac) {
      return false;
    }
    const unsigned = {
      channelId: signal.channelId,
      from: signal.from,
      seq: signal.seq,
      body: signal.body,
    };
    const bytes = new TextEncoder().encode(canonicalSignal(unsigned));
    return crypto.subtle.verify(
      "HMAC",
      hmacKey,
      base64urlToBytes(signal.mac),
      bytes,
    );
  }

  function canonicalSignal(signal) {
    return JSON.stringify({
      channelId: signal.channelId,
      from: signal.from,
      seq: signal.seq,
      body: signal.body,
    });
  }

  function renderFrame(frame, self) {
    if (frame.type === "chat") {
      addMessage(frame.from ?? (self ? state.role : "peer"), frame.text, self);
      return;
    }

    if (frame.type === "status" || frame.type === "error") {
      addNotice(frame.message ?? frame.text ?? JSON.stringify(frame));
      return;
    }

    if (frame.type === "tool_call" || frame.type === "tool_result") {
      addToolFrame(frame, self);
      return;
    }

    addToolFrame(
      {
        type: "event",
        name: frame.type ?? "unknown",
        output: frame,
      },
      self,
    );
  }

  function addMessage(from, text, self) {
    const row = document.createElement("div");
    row.className = `turn${self ? " self" : ""}`;

    const avatar = document.createElement("div");
    avatar.className = "avatar";
    avatar.textContent = initials(from);

    const stack = document.createElement("div");
    stack.className = "message-stack";

    const meta = document.createElement("div");
    meta.className = "message-meta";
    meta.textContent = labelForRole(from);

    const bubble = document.createElement("div");
    bubble.className = "bubble";
    appendTextWithCode(bubble, text);

    stack.append(meta, bubble);
    row.append(avatar, stack);
    els.log.append(row);
    scrollToBottom();
  }

  function addToolFrame(frame, self) {
    const row = document.createElement("div");
    row.className = `turn${self ? " self" : ""}`;

    const avatar = document.createElement("div");
    avatar.className = "avatar";
    avatar.textContent = "tl";

    const stack = document.createElement("div");
    stack.className = "message-stack";

    const meta = document.createElement("div");
    meta.className = "message-meta";
    meta.textContent = frame.type === "tool_result" ? "tool result" : "tool";

    const card = document.createElement("div");
    card.className = "tool-card";

    const header = document.createElement("div");
    header.className = "tool-header";

    const name = document.createElement("span");
    name.textContent = frame.name ?? frame.tool ?? frame.type ?? "tool";

    const stateLabel = document.createElement("span");
    stateLabel.textContent = frame.status ?? frame.type ?? "event";

    const body = document.createElement("pre");
    body.className = "tool-body";
    body.textContent = formatToolBody(frame);

    header.append(name, stateLabel);
    card.append(header, body);
    stack.append(meta, card);
    row.append(avatar, stack);
    els.log.append(row);
    scrollToBottom();
  }

  function addNotice(text) {
    const row = document.createElement("div");
    row.className = "event-row";

    const chip = document.createElement("div");
    chip.className = "event-chip";
    chip.textContent = text;

    row.append(chip);
    els.log.append(row);
    scrollToBottom();
  }

  function appendTextWithCode(parent, text) {
    const parts = String(text ?? "").split(/```/g);
    for (let index = 0; index < parts.length; index += 1) {
      if (index % 2 === 1) {
        const pre = document.createElement("pre");
        pre.textContent = stripCodeLanguage(parts[index]);
        parent.append(pre);
      } else if (parts[index]) {
        const paragraph = document.createElement("p");
        paragraph.textContent = parts[index];
        parent.append(paragraph);
      }
    }
  }

  function stripCodeLanguage(value) {
    return value.replace(/^[a-zA-Z0-9_-]+\n/, "");
  }

  function formatToolBody(frame) {
    const body =
      frame.output ?? frame.result ?? frame.arguments ?? frame.input ?? frame;
    if (typeof body === "string") {
      return body;
    }
    return JSON.stringify(body, null, 2);
  }

  function setPill(el, text, stateClass = "") {
    el.className = `status-pill${stateClass ? ` ${stateClass}` : ""}`;
    el.textContent = text;
  }

  function setComposer(enabled, hint) {
    els.input.disabled = !enabled;
    els.send.disabled = !enabled;
    els.hint.textContent = hint;
  }

  function autosizeComposer() {
    els.input.style.height = "auto";
    els.input.style.height = `${Math.min(180, els.input.scrollHeight)}px`;
  }

  function scrollToBottom() {
    els.scroll.scrollTo({
      top: els.scroll.scrollHeight,
      behavior: "smooth",
    });
  }
})();

function readSession() {
  const params = new URLSearchParams(location.search);
  const pathMatch = location.pathname.match(/^\/chat\/(s|agent)\/([^/]+)\/?$/);
  const role =
    params.get("role") ?? (pathMatch?.[1] === "agent" ? "agent" : "user");

  return {
    channelId: params.get("c") ?? pathMatch?.[2] ?? "",
    role: role === "agent" ? "agent" : "user",
  };
}

function readSecret() {
  const hash = new URLSearchParams(location.hash.slice(1));
  return hash.get("k") ?? "";
}

function labelForRole(role) {
  if (role === "agent") {
    return "exo";
  }
  if (role === "user") {
    return "you";
  }
  return role;
}

function initials(role) {
  if (role === "agent") {
    return "ex";
  }
  if (role === "user") {
    return "yo";
  }
  return String(role ?? "??")
    .slice(0, 2)
    .toLowerCase();
}

function bytesToBase64url(bytes) {
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

function base64urlToBytes(value) {
  const padded = value
    .replaceAll("-", "+")
    .replaceAll("_", "/")
    .padEnd(Math.ceil(value.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}
