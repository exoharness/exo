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
    offerStarted: false,
  };

  const els = {
    title: document.querySelector("#title"),
    role: document.querySelector("#role"),
    signal: document.querySelector("#signal"),
    peer: document.querySelector("#peer"),
    data: document.querySelector("#data"),
    log: document.querySelector("#log"),
    form: document.querySelector("#form"),
    input: document.querySelector("#input"),
    send: document.querySelector("#send"),
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
  setPill(els.role, state.role, "good");
  setPill(els.signal, "signaling: connecting");
  setPill(els.peer, "peer: new");
  setPill(els.data, "datachannel: closed");

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
    const text = els.input.value.trim();
    if (!text || !state.dc || state.dc.readyState !== "open") {
      return;
    }
    sendChat(text);
    els.input.value = "";
  });

  setTimeout(() => {
    if (!state.connected) {
      setPill(els.peer, "peer: failed", "bad");
      addNotice(
        "Could not establish a direct connection. Try switching Wi-Fi/cellular, disabling VPN or Private Relay, and keeping the desktop peer open.",
      );
    }
  }, 12000);

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
        status === "connected" ? "good" : status === "failed" ? "bad" : "",
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
    });

    ws.addEventListener("error", () => {
      setPill(els.signal, "signaling: error", "bad");
    });

    ws.addEventListener("message", async (event) => {
      const message = JSON.parse(event.data);
      if (message.channel === "rendezvous" && message.type === "presence") {
        await maybeStartOffer(message.roles ?? []);
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
      setPill(els.data, "datachannel: open", "good");
      els.input.disabled = false;
      els.send.disabled = false;
      addNotice("Direct WebRTC DataChannel connected.");
    });

    channel.addEventListener("close", () => {
      setPill(els.data, "datachannel: closed", state.connected ? "" : "bad");
      els.input.disabled = true;
      els.send.disabled = true;
    });

    channel.addEventListener("message", (event) => {
      const frame = JSON.parse(event.data);
      if (frame.type === "chat") {
        addMessage(frame.from, frame.text, false);
      }
    });
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
    addMessage(state.role, text, true);
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

  function addMessage(from, text, self) {
    const el = document.createElement("div");
    el.className = `message${self ? " self" : ""}`;
    el.innerHTML = `<div class="meta"></div><div></div>`;
    el.children[0].textContent = from;
    el.children[1].textContent = text;
    els.log.append(el);
    el.scrollIntoView({ block: "end" });
  }

  function addNotice(text) {
    const el = document.createElement("div");
    el.className = "notice";
    el.textContent = text;
    els.log.append(el);
    el.scrollIntoView({ block: "end" });
  }

  function setPill(el, text, stateClass = "") {
    el.className = `pill${stateClass ? ` ${stateClass}` : ""}`;
    el.textContent = text;
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
