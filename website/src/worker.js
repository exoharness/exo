const SESSION_PATH_PATTERN = /^\/chat\/(?:s|agent)\/([A-Za-z0-9_-]{8,128})\/?$/;
const WS_PATH_PATTERN = /^\/chat\/ws\/([A-Za-z0-9_-]{8,128})\/?$/;

const SECURITY_HEADERS = {
  "Referrer-Policy": "no-referrer",
  "X-Content-Type-Options": "nosniff",
  "Content-Security-Policy":
    "default-src 'self'; connect-src 'self' wss:; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'",
};

const rooms = new Map();
const SESSION_TTL_MS = 30 * 60 * 1000;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);

    if (
      url.pathname === "/wrangler.jsonc" ||
      url.pathname.startsWith("/src/")
    ) {
      return new Response("Not found\n", {
        status: 404,
        headers: SECURITY_HEADERS,
      });
    }

    const wsMatch = url.pathname.match(WS_PATH_PATTERN);
    if (wsMatch) {
      return acceptSignalingSocket(request, wsMatch[1], url);
    }

    if (isSessionPage(url)) {
      const chatUrl = new URL("/chat", url);
      const response = await env.ASSETS.fetch(new Request(chatUrl, request));
      return withSecurityHeaders(response);
    }

    if (url.pathname === "/chat" || url.pathname === "/chat/") {
      return new Response(
        "Create a session with scripts/webrtc-rendezvous-demo.mjs\n",
        {
          status: 200,
          headers: {
            ...SECURITY_HEADERS,
            "Content-Type": "text/plain; charset=utf-8",
          },
        },
      );
    }

    const response = await env.ASSETS.fetch(request);
    return withSecurityHeaders(response);
  },
};

function acceptSignalingSocket(request, channelId, url) {
  const role = parseRole(url.searchParams.get("role"));

  if (request.headers.get("Upgrade") !== "websocket") {
    return new Response("Expected WebSocket upgrade\n", {
      status: 426,
      headers: SECURITY_HEADERS,
    });
  }

  if (!role) {
    return new Response("Missing or invalid role\n", {
      status: 400,
      headers: SECURITY_HEADERS,
    });
  }

  const pair = new WebSocketPair();
  const [client, server] = Object.values(pair);
  const room = getRoom(channelId);
  const existing = room.peers.get(role);

  if (existing) {
    existing.socket.close(1000, "Replaced by a newer connection");
  }

  const peer = {
    socket: server,
    role,
    connectedAt: Date.now(),
  };

  room.peers.set(role, peer);
  server.accept();
  server.addEventListener("message", (event) => {
    relaySignal(channelId, peer, event.data);
  });
  server.addEventListener("close", () => {
    removePeer(channelId, peer);
  });
  server.addEventListener("error", () => {
    removePeer(channelId, peer);
  });

  broadcastPresence(room);

  return new Response(null, {
    status: 101,
    webSocket: client,
  });
}

function isSessionPage(url) {
  if (SESSION_PATH_PATTERN.test(url.pathname)) {
    return true;
  }

  if (url.pathname !== "/chat" && url.pathname !== "/chat/") {
    return false;
  }

  return Boolean(
    url.searchParams.get("c") && parseRole(url.searchParams.get("role")),
  );
}

function getRoom(channelId) {
  const now = Date.now();
  let room = rooms.get(channelId);

  if (!room || room.expiresAt <= now) {
    room = {
      channelId,
      expiresAt: now + SESSION_TTL_MS,
      peers: new Map(),
    };
    rooms.set(channelId, room);
  } else {
    room.expiresAt = now + SESSION_TTL_MS;
  }

  cleanupRooms(now);
  return room;
}

function relaySignal(channelId, sender, message) {
  const room = rooms.get(channelId);
  if (!room) {
    sender.socket.close(1000, "Session expired");
    return;
  }

  if (typeof message !== "string") {
    sender.socket.close(1003, "Only text signaling messages are supported");
    return;
  }

  if (message.length > 64 * 1024) {
    sender.socket.close(1009, "Signaling message too large");
    return;
  }

  room.expiresAt = Date.now() + SESSION_TTL_MS;

  for (const peer of room.peers.values()) {
    if (
      peer.role !== sender.role &&
      peer.socket.readyState === WebSocket.OPEN
    ) {
      peer.socket.send(message);
    }
  }
}

function removePeer(channelId, peer) {
  const room = rooms.get(channelId);
  if (!room) {
    return;
  }

  if (room.peers.get(peer.role) === peer) {
    room.peers.delete(peer.role);
  }

  if (room.peers.size === 0) {
    rooms.delete(channelId);
  } else {
    broadcastPresence(room);
  }
}

function broadcastPresence(room) {
  const message = JSON.stringify({
    channel: "rendezvous",
    type: "presence",
    roles: [...room.peers.keys()].sort(),
    at: Date.now(),
  });

  for (const peer of room.peers.values()) {
    if (peer.socket.readyState === WebSocket.OPEN) {
      peer.socket.send(message);
    }
  }
}

function cleanupRooms(now) {
  for (const [channelId, room] of rooms.entries()) {
    if (room.expiresAt > now) {
      continue;
    }

    for (const peer of room.peers.values()) {
      peer.socket.close(1000, "Session expired");
    }
    rooms.delete(channelId);
  }
}

function parseRole(role) {
  if (role === "agent" || role === "user") {
    return role;
  }
  return null;
}

function withSecurityHeaders(response) {
  const headers = new Headers(response.headers);
  for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
    headers.set(name, value);
  }
  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}
