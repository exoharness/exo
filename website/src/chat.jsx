import * as React from "react";
import { createRoot } from "react-dom/client";
import {
  ActivityIcon,
  CircleIcon,
  CopyIcon,
  HammerIcon,
  SendHorizontalIcon,
  WifiIcon,
  WifiOffIcon,
} from "lucide-react";

import {
  Attachment,
  AttachmentContent,
  AttachmentDescription,
  AttachmentTitle,
} from "@/components/ui/attachment";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import { Bubble, BubbleContent } from "@/components/ui/bubble";
import { Button } from "@/components/ui/button";
import { Marker, MarkerContent, MarkerIcon } from "@/components/ui/marker";
import {
  Message,
  MessageAvatar,
  MessageContent,
  MessageFooter,
  MessageHeader,
} from "@/components/ui/message";
import {
  MessageScroller,
  MessageScrollerButton,
  MessageScrollerContent,
  MessageScrollerItem,
  MessageScrollerProvider,
  MessageScrollerViewport,
} from "@/components/ui/message-scroller";
import { Textarea } from "@/components/ui/textarea";
import "./chat.css";

function ChatApp() {
  const session = React.useMemo(() => readSession(), []);
  const [events, setEvents] = React.useState([]);
  const [input, setInput] = React.useState("");
  const [composer, setComposer] = React.useState({
    enabled: false,
    label: "Waiting",
  });
  const [status, setStatus] = React.useState({
    data: { label: "DataChannel", tone: "idle", value: "closed" },
    peer: { label: "Peer", tone: "warning", value: "waiting" },
    role: { label: "Role", tone: "success", value: session.role },
    signal: { label: "Signal", tone: "warning", value: "connecting" },
  });

  const channelRef = React.useRef(null);
  const connectedRef = React.useRef(false);
  const failureTimerRef = React.useRef(null);
  const hmacKeyRef = React.useRef(null);
  const offerStartedRef = React.useRef(false);
  const pcRef = React.useRef(null);
  const remoteSeenRef = React.useRef(false);
  const seqRef = React.useRef(0);
  const textareaRef = React.useRef(null);
  const wsRef = React.useRef(null);

  const addEvent = React.useCallback((event) => {
    setEvents((current) => [
      ...current,
      {
        id: crypto.randomUUID(),
        createdAt: Date.now(),
        ...event,
      },
    ]);
  }, []);

  const addNotice = React.useCallback(
    (text, tone = "neutral") => {
      addEvent({ kind: "notice", text, tone });
    },
    [addEvent],
  );

  const setStatusPart = React.useCallback((key, next) => {
    setStatus((current) => ({
      ...current,
      [key]: {
        ...current[key],
        ...next,
      },
    }));
  }, []);

  React.useEffect(() => {
    const el = textareaRef.current;
    if (!el) {
      return;
    }
    el.style.height = "auto";
    el.style.height = `${Math.min(180, el.scrollHeight)}px`;
  }, [input]);

  React.useEffect(() => {
    if (!session.channelId || !session.secret) {
      addNotice(
        "Missing channel id or secret. Start from the URL printed by the demo script.",
        "danger",
      );
      setComposer({ enabled: false, label: "Missing session" });
      return;
    }

    let disposed = false;

    start().catch((error) => {
      addNotice(errorMessage(error), "danger");
      setStatusPart("signal", { tone: "danger", value: "error" });
      setComposer({ enabled: false, label: "Connection error" });
    });

    return () => {
      disposed = true;
      if (failureTimerRef.current) {
        clearTimeout(failureTimerRef.current);
      }
      channelRef.current?.close();
      wsRef.current?.close();
      pcRef.current?.close();
    };

    async function start() {
      hmacKeyRef.current = await crypto.subtle.importKey(
        "raw",
        base64urlToBytes(session.secret),
        { name: "HMAC", hash: "SHA-256" },
        false,
        ["sign", "verify"],
      );
      if (disposed) {
        return;
      }

      pcRef.current = createPeerConnection();
      wsRef.current = connectSignaling();
    }

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
        const value = pc.connectionState;
        setStatusPart("peer", {
          tone:
            value === "connected"
              ? "success"
              : value === "failed" || value === "disconnected"
                ? "danger"
                : "warning",
          value,
        });
      });

      if (session.role === "user") {
        attachDataChannel(pc.createDataChannel("exo", { ordered: true }));
      } else {
        pc.addEventListener("datachannel", (event) => {
          attachDataChannel(event.channel);
        });
      }

      return pc;
    }

    function connectSignaling() {
      const wsUrl = new URL(`/chat/ws/${session.channelId}`, location.href);
      wsUrl.protocol = location.protocol === "https:" ? "wss:" : "ws:";
      wsUrl.searchParams.set("role", session.role);

      const ws = new WebSocket(wsUrl);

      ws.addEventListener("open", async () => {
        setStatusPart("signal", { tone: "success", value: "open" });
        await sendSignal({
          type: "hello",
          role: session.role,
          nonce: bytesToBase64url(crypto.getRandomValues(new Uint8Array(16))),
        });
        addNotice(
          session.role === "user"
            ? "Waiting for the desktop peer..."
            : "Waiting for the phone peer...",
        );
      });

      ws.addEventListener("close", () => {
        setStatusPart("signal", {
          tone: connectedRef.current ? "idle" : "danger",
          value: "closed",
        });
        setComposer({ enabled: false, label: "Closed" });
      });

      ws.addEventListener("error", () => {
        setStatusPart("signal", { tone: "danger", value: "error" });
        setComposer({ enabled: false, label: "Error" });
      });

      ws.addEventListener("message", async (event) => {
        const message = JSON.parse(event.data);
        if (message.channel === "rendezvous" && message.type === "presence") {
          await handlePresence(message.roles ?? []);
          return;
        }

        if (!(await verifySignedSignal(message))) {
          addNotice(
            "Rejected a signaling message with an invalid signature.",
            "danger",
          );
          return;
        }

        await handleSignal(message.body);
      });

      return ws;
    }

    async function handlePresence(roles) {
      const remoteRole = session.role === "agent" ? "user" : "agent";
      if (roles.includes(remoteRole) && !remoteSeenRef.current) {
        remoteSeenRef.current = true;
        setStatusPart("peer", { tone: "warning", value: "connecting" });
        addNotice(
          session.role === "agent"
            ? "Phone peer joined. Starting direct connection..."
            : "Desktop peer joined. Starting direct connection...",
        );
        startFailureTimer();
      }

      await maybeStartOffer(roles);
    }

    function startFailureTimer() {
      if (failureTimerRef.current) {
        clearTimeout(failureTimerRef.current);
      }

      failureTimerRef.current = setTimeout(() => {
        if (!connectedRef.current) {
          setStatusPart("peer", { tone: "danger", value: "failed" });
          setComposer({ enabled: false, label: "Failed" });
          addNotice(
            "Could not establish a direct connection. Try switching Wi-Fi/cellular, disabling VPN or Private Relay, and keeping the desktop peer open.",
            "danger",
          );
        }
      }, 12000);
    }

    async function maybeStartOffer(roles) {
      if (
        session.role !== "user" ||
        offerStartedRef.current ||
        !roles.includes("agent") ||
        pcRef.current?.signalingState !== "stable"
      ) {
        return;
      }

      offerStartedRef.current = true;
      const offer = await pcRef.current.createOffer();
      await pcRef.current.setLocalDescription(offer);
      await sendSignal({ type: "offer", sdp: offer.sdp });
    }

    async function handleSignal(body) {
      if (body.type === "hello") {
        return;
      }

      if (body.type === "offer" && session.role === "agent") {
        await pcRef.current.setRemoteDescription({
          type: "offer",
          sdp: body.sdp,
        });
        const answer = await pcRef.current.createAnswer();
        await pcRef.current.setLocalDescription(answer);
        await sendSignal({ type: "answer", sdp: answer.sdp });
        return;
      }

      if (body.type === "answer" && session.role === "user") {
        await pcRef.current.setRemoteDescription({
          type: "answer",
          sdp: body.sdp,
        });
        return;
      }

      if (body.type === "candidate" && body.candidate) {
        await pcRef.current.addIceCandidate(body.candidate);
      }
    }

    function attachDataChannel(channel) {
      channelRef.current = channel;

      channel.addEventListener("open", () => {
        connectedRef.current = true;
        if (failureTimerRef.current) {
          clearTimeout(failureTimerRef.current);
          failureTimerRef.current = null;
        }
        setStatusPart("peer", { tone: "success", value: "connected" });
        setStatusPart("data", { tone: "success", value: "open" });
        setComposer({ enabled: true, label: "Ready" });
        addNotice("Direct WebRTC DataChannel connected.", "success");
      });

      channel.addEventListener("close", () => {
        setStatusPart("data", {
          tone: connectedRef.current ? "idle" : "danger",
          value: "closed",
        });
        setComposer({ enabled: false, label: "Closed" });
      });

      channel.addEventListener("message", (event) => {
        renderFrame(JSON.parse(event.data), false);
      });
    }

    async function sendSignal(body) {
      const signal = {
        channelId: session.channelId,
        from: session.role,
        seq: ++seqRef.current,
        body,
      };
      const mac = await signSignal(signal);
      wsRef.current?.send(JSON.stringify({ ...signal, mac }));
    }

    async function signSignal(signal) {
      const bytes = new TextEncoder().encode(canonicalSignal(signal));
      const signature = await crypto.subtle.sign(
        "HMAC",
        hmacKeyRef.current,
        bytes,
      );
      return bytesToBase64url(new Uint8Array(signature));
    }

    async function verifySignedSignal(signal) {
      if (!signal || signal.channelId !== session.channelId || !signal.mac) {
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
        hmacKeyRef.current,
        base64urlToBytes(signal.mac),
        bytes,
      );
    }

    function renderFrame(frame, self) {
      if (frame.type === "chat") {
        addEvent({
          frame,
          kind: "message",
          self,
          text: frame.text,
          from: frame.from ?? (self ? session.role : "peer"),
        });
        return;
      }

      if (frame.type === "status" || frame.type === "error") {
        addNotice(
          frame.message ?? frame.text ?? JSON.stringify(frame),
          frame.type === "error" ? "danger" : "neutral",
        );
        return;
      }

      addEvent({
        frame,
        kind: "tool",
        self,
      });
    }
  }, [addEvent, addNotice, session, setStatusPart]);

  const sendChat = React.useCallback(() => {
    const text = input.trim();
    const channel = channelRef.current;
    if (!text || !channel || channel.readyState !== "open") {
      return;
    }

    const frame = {
      type: "chat",
      id: crypto.randomUUID(),
      from: session.role,
      text,
      createdAt: Date.now(),
    };
    channel.send(JSON.stringify(frame));
    addEvent({
      frame,
      from: session.role,
      kind: "message",
      self: true,
      text,
    });
    setInput("");
  }, [addEvent, input, session.role]);

  const submitComposer = React.useCallback(
    (event) => {
      event.preventDefault();
      sendChat();
    },
    [sendChat],
  );

  const title =
    session.role === "agent"
      ? "exo chat - desktop peer"
      : "exo chat - phone peer";

  return (
    <div className="chat-shell">
      <header className="topbar">
        <div className="identity">
          <Avatar className="brand-avatar">
            <AvatarFallback>ex</AvatarFallback>
          </Avatar>
          <div className="identity-copy">
            <h1>{title}</h1>
            <p>session {session.channelId?.slice(0, 8) || "missing"}</p>
          </div>
        </div>

        <div className="status-rail" aria-label="Connection status">
          <StatusBadge icon={CircleIcon} status={status.role} />
          <StatusBadge icon={WifiIcon} status={status.signal} />
          <StatusBadge icon={ActivityIcon} status={status.peer} />
          <StatusBadge icon={WifiOffIcon} status={status.data} />
        </div>
      </header>

      <MessageScrollerProvider autoScroll defaultScrollPosition="end">
        <MessageScroller>
          <MessageScrollerViewport>
            <MessageScrollerContent className="conversation-inner">
              {events.length === 0 ? <EmptyState role={session.role} /> : null}
              {events.map((event) => (
                <MessageScrollerItem
                  key={event.id}
                  messageId={event.id}
                  scrollAnchor={event.kind !== "notice"}
                >
                  <ConversationEvent event={event} />
                </MessageScrollerItem>
              ))}
            </MessageScrollerContent>
          </MessageScrollerViewport>
          <MessageScrollerButton
            aria-label="Scroll to latest"
            direction="end"
          />
        </MessageScroller>
      </MessageScrollerProvider>

      <form className="composer" onSubmit={submitComposer}>
        <div className="composer-surface">
          <Textarea
            ref={textareaRef}
            autoComplete="off"
            disabled={!composer.enabled}
            onChange={(event) => setInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                sendChat();
              }
            }}
            placeholder="Message exo..."
            rows={1}
            value={input}
          />
          <div className="composer-bar">
            <div className="composer-tools">
              <Badge
                className={`composer-status composer-status-${composer.enabled ? "success" : "idle"}`}
                variant={composer.enabled ? "outline" : "secondary"}
              >
                {composer.label}
              </Badge>
            </div>
            <Button
              aria-label="Send message"
              disabled={!composer.enabled || !input.trim()}
              size="icon"
              type="submit"
            >
              <SendHorizontalIcon />
            </Button>
          </div>
        </div>
      </form>
    </div>
  );
}

function StatusBadge({ icon: Icon, status }) {
  return (
    <Badge
      className={`status-badge status-badge-${status.tone}`}
      variant={status.tone === "idle" ? "secondary" : "outline"}
    >
      <Icon />
      <span>{status.label}</span>
      <strong>{status.value}</strong>
    </Badge>
  );
}

function EmptyState({ role }) {
  return (
    <div className="empty-state">
      <div className="empty-icon">
        <ActivityIcon />
      </div>
      <h2>{role === "agent" ? "Waiting for phone" : "Waiting for desktop"}</h2>
    </div>
  );
}

function ConversationEvent({ event }) {
  if (event.kind === "notice") {
    return (
      <Marker
        className={`notice-marker notice-marker-${event.tone ?? "neutral"}`}
        role={event.tone === "danger" ? "alert" : "status"}
        variant={event.tone === "neutral" ? "separator" : "default"}
      >
        <MarkerIcon>
          <ActivityIcon />
        </MarkerIcon>
        <MarkerContent>{event.text}</MarkerContent>
      </Marker>
    );
  }

  if (event.kind === "tool") {
    return <ToolMessage event={event} />;
  }

  return <ChatMessage event={event} />;
}

function ChatMessage({ event }) {
  const align = event.self ? "end" : "start";
  const from = event.from ?? (event.self ? "user" : "agent");

  return (
    <Message align={align}>
      <MessageAvatar>
        <Avatar>
          <AvatarFallback>{initials(from)}</AvatarFallback>
        </Avatar>
      </MessageAvatar>
      <MessageContent>
        <MessageHeader className="message-meta">
          <span>{labelForRole(from)}</span>
          <time>{formatTime(event.createdAt)}</time>
        </MessageHeader>
        <Bubble align={align} variant={event.self ? "default" : "secondary"}>
          <BubbleContent>
            <RichText text={event.text} />
          </BubbleContent>
        </Bubble>
        <MessageFooter className="message-meta">
          <Button
            aria-label="Copy message"
            onClick={() => copyText(event.text)}
            size="icon-xs"
            type="button"
            variant="ghost"
          >
            <CopyIcon />
          </Button>
          {event.self ? <span className="message-state">sent</span> : null}
        </MessageFooter>
      </MessageContent>
    </Message>
  );
}

function ToolMessage({ event }) {
  const frame = event.frame ?? {};
  const name = frame.name ?? frame.tool ?? frame.type ?? "tool";
  const status = frame.status ?? frame.type ?? "event";

  return (
    <Message align={event.self ? "end" : "start"}>
      <MessageAvatar>
        <Avatar>
          <AvatarFallback>
            <HammerIcon />
          </AvatarFallback>
        </Avatar>
      </MessageAvatar>
      <MessageContent>
        <MessageHeader className="message-meta">
          <span>{name}</span>
          <Badge variant={status === "error" ? "destructive" : "secondary"}>
            {status}
          </Badge>
        </MessageHeader>
        <Attachment className="tool-attachment">
          <AttachmentContent>
            <AttachmentTitle>{name}</AttachmentTitle>
            <AttachmentDescription>tool frame</AttachmentDescription>
            <pre>{formatToolBody(frame)}</pre>
          </AttachmentContent>
        </Attachment>
      </MessageContent>
    </Message>
  );
}

function RichText({ text }) {
  const parts = String(text ?? "").split(/```/g);
  return parts.map((part, index) => {
    if (index % 2 === 1) {
      return <pre key={index}>{stripCodeLanguage(part)}</pre>;
    }
    return part ? <p key={index}>{part}</p> : null;
  });
}

function canonicalSignal(signal) {
  return JSON.stringify({
    channelId: signal.channelId,
    from: signal.from,
    seq: signal.seq,
    body: signal.body,
  });
}

function readSession() {
  const params = new URLSearchParams(location.search);
  const pathMatch = location.pathname.match(/^\/chat\/(s|agent)\/([^/]+)\/?$/);
  const role =
    params.get("role") ?? (pathMatch?.[1] === "agent" ? "agent" : "user");

  return {
    channelId: params.get("c") ?? pathMatch?.[2] ?? "",
    role: role === "agent" ? "agent" : "user",
    secret: readSecret(),
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
    return "EX";
  }
  if (role === "user") {
    return "YO";
  }
  return String(role ?? "??")
    .slice(0, 2)
    .toUpperCase();
}

function formatTime(value) {
  return new Intl.DateTimeFormat(undefined, {
    hour: "numeric",
    minute: "2-digit",
  }).format(value);
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

async function copyText(value) {
  try {
    await navigator.clipboard.writeText(String(value ?? ""));
  } catch (error) {
    console.warn("Unable to copy message", error);
  }
}

function errorMessage(error) {
  return error instanceof Error ? error.message : String(error);
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

const rootElement = document.querySelector("#root");
rootElement.__exoChatRoot ??= createRoot(rootElement);
rootElement.__exoChatRoot.render(<ChatApp />);
