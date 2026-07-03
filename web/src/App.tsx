import {
  useEffect,
  useMemo,
  useState,
  type CSSProperties,
  type KeyboardEvent,
} from "react";
import { ExoClient, normalizeRequestEndpoint } from "./api/exoClient";
import type {
  AgentRecord,
  BindingRecord,
  ConversationHandleInfo,
  SecretMetadata,
} from "./api/protocol";
import { CommandPalette } from "./components/CommandPalette";
import { ShortcutsOverlay } from "./components/ShortcutsOverlay";
import { HealthBadge, type HealthState } from "./components/HealthBadge";
import { Overview } from "./components/Overview";
import { SidePanels } from "./components/SidePanels";
import { Transcript } from "./components/Transcript";
import { shortId } from "./lib/rendering";
import { eventIdToTimestamp, formatRelativeTime } from "./lib/eventTime";
import {
  computeConversationRollup,
  formatRollupChips,
} from "./lib/conversationStats";
import { deriveSandboxState } from "./lib/sandbox";
import { loadAgentMemory, type MemoryEntry } from "./lib/agentMemory";
import { useConversationEvents } from "./lib/useConversationEvents";
import {
  useConversationStats,
  type ConversationTurnCounts,
} from "./lib/useConversationStats";
import {
  clearActiveChatTurnIfMatch,
  createActiveChatTurn,
  extractChatError,
  markChatCancelRequested,
  makeChatRequestId,
  parseJsonObject,
  type ActiveChatTurn,
} from "./lib/chatBridge";
import { SkeletonRows } from "./components/SkeletonRows";

const DEFAULT_BASE_URL = "/exo";
const BASE_URL_STORAGE_KEY = "exo-base-url";

type AppView = "chat" | "overview";

function readStoredBaseUrl(): string {
  if (typeof window === "undefined") {
    return DEFAULT_BASE_URL;
  }
  try {
    const stored = window.localStorage.getItem(BASE_URL_STORAGE_KEY);
    if (!stored?.trim()) {
      return DEFAULT_BASE_URL;
    }
    normalizeRequestEndpoint(stored);
    return stored.trim();
  } catch {
    return DEFAULT_BASE_URL;
  }
}

export default function App() {
  const [baseUrlInput, setBaseUrlInput] = useState(readStoredBaseUrl);
  const [activeBaseUrl, setActiveBaseUrl] = useState(readStoredBaseUrl);
  const [refreshToken, setRefreshToken] = useState(0);
  const [theme, setTheme] = useState<"light" | "dark">(() =>
    typeof window !== "undefined" &&
    window.localStorage.getItem("exo-theme") === "dark"
      ? "dark"
      : "light",
  );
  const [appView, setAppView] = useState<AppView>("chat");

  useEffect(() => {
    const root = document.documentElement;
    if (theme === "dark") {
      root.setAttribute("data-theme", "dark");
    } else {
      root.removeAttribute("data-theme");
    }
    window.localStorage.setItem("exo-theme", theme);
  }, [theme]);

  const [paletteOpen, setPaletteOpen] = useState(false);
  const [shortcutsOpen, setShortcutsOpen] = useState(false);

  useEffect(() => {
    function onKeyDown(event: globalThis.KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((current) => !current);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  useEffect(() => {
    function onKeyDown(event: globalThis.KeyboardEvent) {
      if (event.key !== "?" || event.metaKey || event.ctrlKey || event.altKey) {
        return;
      }
      const target = event.target;
      if (target instanceof HTMLElement) {
        const tag = target.tagName;
        if (
          tag === "INPUT" ||
          tag === "TEXTAREA" ||
          tag === "SELECT" ||
          target.isContentEditable
        ) {
          return;
        }
      }
      event.preventDefault();
      setShortcutsOpen((current) => !current);
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  const [health, setHealth] = useState<HealthState>({
    status: "idle",
    message: "not checked",
  });
  const [appError, setAppError] = useState<string | null>(null);

  const [agents, setAgents] = useState<AgentRecord[]>([]);
  const [conversations, setConversations] = useState<ConversationHandleInfo[]>(
    [],
  );
  const [selectedAgentId, setSelectedAgentId] = useState<string | null>(null);
  const [selectedConversationId, setSelectedConversationId] = useState<
    string | null
  >(null);
  const [activeChatTurn, setActiveChatTurn] = useState<ActiveChatTurn | null>(
    null,
  );
  const [turnElapsedSeconds, setTurnElapsedSeconds] = useState(0);

  const [rootSecrets, setRootSecrets] = useState<SecretMetadata[]>([]);
  const [agentSecrets, setAgentSecrets] = useState<SecretMetadata[]>([]);
  const [agentMemory, setAgentMemory] = useState<MemoryEntry[] | null>(null);
  const [conversationSecrets, setConversationSecrets] = useState<
    SecretMetadata[]
  >([]);
  const [rootBindings, setRootBindings] = useState<BindingRecord[]>([]);
  const [agentBindings, setAgentBindings] = useState<BindingRecord[]>([]);
  const [conversationBindings, setConversationBindings] = useState<
    BindingRecord[]
  >([]);

  const [loadingAgents, setLoadingAgents] = useState(false);
  const [loadingConversations, setLoadingConversations] = useState(false);
  const [loadingDetails, setLoadingDetails] = useState(false);
  const [transcriptError, setTranscriptError] = useState<string | null>(null);

  const clientState = useMemo(() => {
    try {
      return { client: new ExoClient(activeBaseUrl), error: null };
    } catch (error) {
      return {
        client: null,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }, [activeBaseUrl]);
  const client = clientState.client;
  const turnPending = activeChatTurn != null;
  const conversationTurnCounts = useConversationStats(
    client,
    selectedAgentId,
    conversations,
  );

  const {
    events,
    loading: loadingEvents,
    error: eventsError,
    poll: pollEvents,
  } = useConversationEvents({
    client,
    agentId: selectedAgentId,
    conversationId: selectedConversationId,
    turnPending,
    reloadKey: refreshToken,
  });

  const selectedConversation = useMemo(
    () =>
      conversations.find(
        (conversation) => conversation.record.id === selectedConversationId,
      ) ?? null,
    [conversations, selectedConversationId],
  );

  const selectedAgent = useMemo(
    () => agents.find((agent) => agent.id === selectedAgentId) ?? null,
    [agents, selectedAgentId],
  );

  const sandboxes = useMemo(() => deriveSandboxState(events), [events]);
  const conversationRollup = useMemo(
    () => computeConversationRollup(events),
    [events],
  );
  const conversationRollupChips = useMemo(
    () => formatRollupChips(conversationRollup),
    [conversationRollup],
  );

  const scopedSecrets = useMemo(
    () => ({
      global: rootSecrets,
      agent: agentSecrets,
      conversation: conversationSecrets,
    }),
    [agentSecrets, conversationSecrets, rootSecrets],
  );

  const scopedBindings = useMemo(
    () => ({
      global: rootBindings,
      agent: agentBindings,
      conversation: conversationBindings,
    }),
    [agentBindings, conversationBindings, rootBindings],
  );

  useEffect(() => {
    if (!client) {
      setAgents([]);
      setConversations([]);
      setRootSecrets([]);
      setAgentSecrets([]);
      setConversationSecrets([]);
      setRootBindings([]);
      setAgentBindings([]);
      setConversationBindings([]);
      setLoadingAgents(false);
      setLoadingConversations(false);
      setLoadingDetails(false);
      setHealth({
        status: "error",
        message: clientState.error || "invalid exoharness base URL",
      });
      setAppError(clientState.error || "Invalid exoharness base URL.");
      return;
    }
    const exoClient = client;

    let cancelled = false;
    const controller = new AbortController();
    const timeout = window.setTimeout(() => controller.abort(), 3000);

    setHealth({ status: "checking", message: "checking /health" });
    setAppError(null);
    setLoadingAgents(true);
    setLoadingDetails(true);

    async function loadInitial() {
      try {
        const healthText = await exoClient.health(controller.signal);
        if (!cancelled) {
          setHealth({ status: "ok", message: healthText.trim() || "ok" });
        }
      } catch (error) {
        if (!cancelled) {
          setHealth({
            status: "error",
            message: error instanceof Error ? error.message : String(error),
          });
        }
      } finally {
        window.clearTimeout(timeout);
      }

      const [agentResult, secretResult, bindingResult] =
        await Promise.allSettled([
          exoClient.listAgents(),
          exoClient.listRootSecrets(),
          exoClient.listRootBindings(),
        ]);

      if (cancelled) {
        return;
      }

      if (agentResult.status === "fulfilled") {
        setAgents(agentResult.value);
        setSelectedAgentId((current) => {
          if (
            current &&
            agentResult.value.some((agent) => agent.id === current)
          ) {
            return current;
          }
          return agentResult.value[0]?.id ?? null;
        });
      } else {
        setAgents([]);
        setSelectedAgentId(null);
        setAppError(
          agentResult.reason instanceof Error
            ? agentResult.reason.message
            : String(agentResult.reason),
        );
      }

      setRootSecrets(
        secretResult.status === "fulfilled" ? secretResult.value : [],
      );
      setRootBindings(
        bindingResult.status === "fulfilled" ? bindingResult.value : [],
      );
      setLoadingAgents(false);
      setLoadingDetails(false);
    }

    void loadInitial();

    return () => {
      cancelled = true;
      controller.abort();
      window.clearTimeout(timeout);
    };
  }, [client, clientState.error, refreshToken]);

  useEffect(() => {
    if (!client || !selectedAgentId) {
      setConversations([]);
      setSelectedConversationId(null);
      setAgentSecrets([]);
      setAgentBindings([]);
      setAgentMemory(null);
      setConversationSecrets([]);
      setConversationBindings([]);
      setLoadingConversations(false);
      setLoadingDetails(false);
      return;
    }
    const exoClient = client;
    const agentId = selectedAgentId;

    let cancelled = false;
    setLoadingConversations(true);
    setLoadingDetails(true);
    setTranscriptError(null);

    async function loadAgentSelection() {
      const [conversationResult, secretResult, bindingResult, memoryResult] =
        await Promise.allSettled([
          exoClient.listConversations(agentId),
          exoClient.listAgentSecrets(agentId),
          exoClient.listAgentBindings(agentId),
          loadAgentMemory(exoClient, agentId),
        ]);

      if (cancelled) {
        return;
      }

      if (conversationResult.status === "fulfilled") {
        setConversations(conversationResult.value);
        setSelectedConversationId((current) => {
          if (
            current &&
            conversationResult.value.some(
              (conversation) => conversation.record.id === current,
            )
          ) {
            return current;
          }
          return conversationResult.value[0]?.record.id ?? null;
        });
      } else {
        setConversations([]);
        setSelectedConversationId(null);
        setTranscriptError(
          conversationResult.reason instanceof Error
            ? conversationResult.reason.message
            : String(conversationResult.reason),
        );
      }

      setAgentSecrets(
        secretResult.status === "fulfilled" ? secretResult.value : [],
      );
      setAgentBindings(
        bindingResult.status === "fulfilled" ? bindingResult.value : [],
      );
      setAgentMemory(
        memoryResult.status === "fulfilled" ? memoryResult.value : null,
      );
      setLoadingConversations(false);
      setLoadingDetails(false);
    }

    void loadAgentSelection();

    return () => {
      cancelled = true;
    };
  }, [client, refreshToken, selectedAgentId]);

  useEffect(() => {
    if (!client || !selectedAgentId || !selectedConversationId) {
      setConversationSecrets([]);
      setConversationBindings([]);
      setLoadingDetails(false);
      return;
    }
    const exoClient = client;
    const agentId = selectedAgentId;
    const conversationId = selectedConversationId;

    let cancelled = false;
    setLoadingDetails(true);

    async function loadConversationDetails() {
      const [secretResult, bindingResult] = await Promise.allSettled([
        exoClient.listConversationSecrets(agentId, conversationId),
        exoClient.listConversationBindings(agentId, conversationId),
      ]);

      if (cancelled) {
        return;
      }

      setConversationSecrets(
        secretResult.status === "fulfilled" ? secretResult.value : [],
      );
      setConversationBindings(
        bindingResult.status === "fulfilled" ? bindingResult.value : [],
      );
      setLoadingDetails(false);
    }

    void loadConversationDetails();

    return () => {
      cancelled = true;
    };
  }, [client, refreshToken, selectedAgentId, selectedConversationId]);

  useEffect(() => {
    if (!activeChatTurn) {
      setTurnElapsedSeconds(0);
      return;
    }

    const startedAt = activeChatTurn.startedAt;
    function updateElapsed() {
      setTurnElapsedSeconds(
        Math.max(0, Math.floor((Date.now() - startedAt) / 1000)),
      );
    }

    updateElapsed();
    const interval = window.setInterval(updateElapsed, 1000);
    return () => window.clearInterval(interval);
  }, [activeChatTurn]);

  function handleRefresh() {
    setRefreshToken((value) => value + 1);
  }

  function commitBaseUrl() {
    const trimmed = baseUrlInput.trim() || DEFAULT_BASE_URL;
    try {
      normalizeRequestEndpoint(trimmed);
    } catch {
      setBaseUrlInput(activeBaseUrl);
      return;
    }
    if (trimmed === activeBaseUrl) {
      setBaseUrlInput(trimmed);
      return;
    }
    setActiveBaseUrl(trimmed);
    setBaseUrlInput(trimmed);
    window.localStorage.setItem(BASE_URL_STORAGE_KEY, trimmed);
    setRefreshToken((value) => value + 1);
  }

  function handleBaseUrlKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (event.key === "Enter") {
      event.currentTarget.blur();
    }
  }

  async function handleSendChatMessage(message: string) {
    if (!selectedAgentId || !selectedConversationId) {
      throw new Error("select an agent and conversation before sending");
    }

    const requestId = makeChatRequestId();
    // The turn runs server-side; the live poll renders it as it lands. Mark the
    // turn pending so polling speeds up, then poll once more the moment it ends.
    setActiveChatTurn(createActiveChatTurn(requestId));
    try {
      await sendChatTurn({
        agent: selectedAgentId,
        conversation: selectedConversationId,
        message,
        requestId,
      });
    } finally {
      setActiveChatTurn((current) =>
        clearActiveChatTurnIfMatch(current, requestId),
      );
      pollEvents();
    }
  }

  async function handleCancelChatTurn() {
    const requestId = activeChatTurn?.requestId;
    if (!requestId) {
      return;
    }

    setActiveChatTurn((current) => markChatCancelRequested(current, requestId));
    await cancelChatTurn(requestId);
    pollEvents();
  }

  async function handleCreateConversation(name: string) {
    if (!selectedAgentId) {
      throw new Error("select an agent before creating a conversation");
    }
    // Creating a conversation is a write, which the read-only substrate transport
    // does not do, so it goes through the same local bridge as sending a turn.
    const created = await createConversationViaBridge({
      agent: selectedAgentId,
      name,
    });
    if (created.id) {
      setSelectedConversationId(created.id);
    }
    setRefreshToken((value) => value + 1);
  }

  function handleSelectAgent(agentId: string) {
    setSelectedAgentId(agentId);
    setSelectedConversationId(null);
    setConversationSecrets([]);
    setConversationBindings([]);
  }

  function handleOverviewSelect(agentId: string, conversationId: string) {
    setSelectedAgentId(agentId);
    setSelectedConversationId(conversationId);
    setAppView("chat");
  }

  const loadedOverviewEvents = useMemo(() => {
    if (!selectedAgentId || !selectedConversationId) {
      return null;
    }
    return {
      agentId: selectedAgentId,
      conversationId: selectedConversationId,
      events,
    };
  }, [events, selectedAgentId, selectedConversationId]);

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="topbar-left">
          <div className="wordmark">exo web</div>
          <label className="base-url-control">
            <span>base</span>
            <input
              aria-label="Exoharness base URL"
              onBlur={commitBaseUrl}
              onChange={(event) => setBaseUrlInput(event.target.value)}
              onKeyDown={handleBaseUrlKeyDown}
              spellCheck={false}
              value={baseUrlInput}
            />
          </label>
        </div>

        <div className="topbar-right">
          <div
            className="view-toggle"
            role="tablist"
            aria-label="Workspace view"
          >
            <button
              aria-selected={appView === "chat"}
              className={`view-toggle-button ${appView === "chat" ? "is-active" : ""}`}
              onClick={() => setAppView("chat")}
              role="tab"
              type="button"
            >
              Chat
            </button>
            <button
              aria-selected={appView === "overview"}
              className={`view-toggle-button ${appView === "overview" ? "is-active" : ""}`}
              onClick={() => setAppView("overview")}
              role="tab"
              type="button"
            >
              Overview
            </button>
          </div>
          <HealthBadge health={health} />
          <span className="readonly-status">read-only inspector</span>
          <button
            aria-label={`Switch to ${theme === "dark" ? "light" : "dark"} mode`}
            className="theme-toggle"
            onClick={() =>
              setTheme((current) => (current === "dark" ? "light" : "dark"))
            }
            title={`Switch to ${theme === "dark" ? "light" : "dark"} mode`}
            type="button"
          >
            {theme === "dark" ? <SunIcon /> : <MoonIcon />}
          </button>
        </div>
      </header>

      {appError ? <div className="app-error">{appError}</div> : null}

      {appView === "overview" ? (
        <Overview
          agents={agents}
          client={client}
          loadedEvents={loadedOverviewEvents}
          onSelectConversation={handleOverviewSelect}
          refreshToken={refreshToken}
        />
      ) : (
        <div className="workspace">
          <LeftNav
            agents={agents}
            conversations={conversations}
            turnCounts={conversationTurnCounts}
            loadingAgents={loadingAgents}
            loadingConversations={loadingConversations}
            selectedAgent={selectedAgent}
            selectedAgentId={selectedAgentId}
            selectedConversationId={selectedConversationId}
            onSelectAgent={handleSelectAgent}
            onSelectConversation={setSelectedConversationId}
            onCreateConversation={handleCreateConversation}
          />
          <Transcript
            agentLabel={
              selectedAgent?.slug || selectedAgent?.name || "assistant"
            }
            agentId={selectedAgentId}
            client={client}
            conversation={selectedConversation}
            conversationId={selectedConversationId}
            events={events}
            loading={loadingEvents}
            error={transcriptError ?? eventsError}
            canChat={Boolean(selectedAgentId && selectedConversationId)}
            selectionKey={`${selectedAgentId ?? "none"}:${selectedConversationId ?? "none"}`}
            turnPending={turnPending}
            turnCancelPending={activeChatTurn?.cancelRequested ?? false}
            turnElapsedSeconds={turnElapsedSeconds}
            onCancelChatTurn={handleCancelChatTurn}
            onSendChatMessage={handleSendChatMessage}
          />
          <SidePanels
            secrets={scopedSecrets}
            bindings={scopedBindings}
            sandboxes={sandboxes}
            memory={agentMemory}
            loading={loadingDetails}
            rollupChips={conversationRollupChips}
            onRefresh={handleRefresh}
          />
        </div>
      )}

      <CommandPalette
        open={paletteOpen}
        onClose={() => setPaletteOpen(false)}
        theme={theme}
        agents={agents}
        conversations={conversations}
        selectedAgentId={selectedAgentId}
        selectedConversationId={selectedConversationId}
        selectedConversation={selectedConversation}
        events={events}
        onSelectAgent={handleSelectAgent}
        onSelectConversation={setSelectedConversationId}
        onToggleTheme={() =>
          setTheme((current) => (current === "dark" ? "light" : "dark"))
        }
        onScrollToLatest={scrollTranscriptToLatest}
      />
      <ShortcutsOverlay
        open={shortcutsOpen}
        onClose={() => setShortcutsOpen(false)}
      />
    </div>
  );
}

function scrollTranscriptToLatest() {
  const el = document.querySelector(".transcript-scroll");
  if (el instanceof HTMLElement) {
    el.scrollTop = el.scrollHeight;
  }
}

// A stable per-conversation colour derived from its id, so the same conversation
// always reads the same hue and different ones are easy to tell apart at a glance.
// Mid saturation/lightness keeps every hue legible on both the light and dark nav.
function conversationColor(id: string): string {
  let hash = 0;
  for (let index = 0; index < id.length; index += 1) {
    hash = (hash * 31 + id.charCodeAt(index)) >>> 0;
  }
  return `hsl(${hash % 360} 62% 56%)`;
}

async function sendChatTurn({
  agent,
  conversation,
  message,
  requestId,
}: {
  agent: string;
  conversation: string;
  message: string;
  requestId: string;
}) {
  const response = await fetch("/chat", {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/json",
    },
    body: JSON.stringify({ agent, conversation, message, requestId }),
  });

  const text = await response.text();
  const payload = parseJsonObject(text);

  if (!response.ok) {
    throw new Error(
      extractChatError(payload) || `chat bridge failed (${response.status})`,
    );
  }

  if (payload && payload.ok === true) {
    return;
  }

  const stderr =
    payload && typeof payload.stderr === "string" && payload.stderr.trim()
      ? `: ${payload.stderr.trim()}`
      : "";
  throw new Error(
    (extractChatError(payload) || "chat bridge did not complete the turn") +
      stderr,
  );
}

async function createConversationViaBridge({
  agent,
  name,
}: {
  agent: string;
  name: string;
}): Promise<{ id: string | null; slug: string | null }> {
  const response = await fetch("/chat/create", {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/json",
    },
    body: JSON.stringify({ agent, name }),
  });

  const text = await response.text();
  const payload = parseJsonObject(text);

  if (!response.ok || !(payload && payload.ok === true)) {
    throw new Error(
      extractChatError(payload) ||
        `conversation bridge failed (${response.status})`,
    );
  }

  return {
    id: typeof payload.id === "string" ? payload.id : null,
    slug: typeof payload.slug === "string" ? payload.slug : null,
  };
}

async function cancelChatTurn(requestId: string) {
  const response = await fetch("/chat/cancel", {
    method: "POST",
    headers: {
      accept: "application/json",
      "content-type": "application/json",
    },
    body: JSON.stringify({ requestId }),
  });

  const text = await response.text();
  const payload = parseJsonObject(text);

  if (!response.ok || !payload || payload.ok !== true) {
    throw new Error(
      extractChatError(payload) || `chat cancel failed (${response.status})`,
    );
  }
}

function SunIcon() {
  return (
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      aria-hidden="true"
    >
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
    </svg>
  );
}

function MoonIcon() {
  return (
    <svg
      width="15"
      height="15"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
    </svg>
  );
}

function LeftNav({
  agents,
  conversations,
  turnCounts,
  loadingAgents,
  loadingConversations,
  selectedAgent,
  selectedAgentId,
  selectedConversationId,
  onSelectAgent,
  onSelectConversation,
  onCreateConversation,
}: {
  agents: AgentRecord[];
  conversations: ConversationHandleInfo[];
  turnCounts: ConversationTurnCounts;
  loadingAgents: boolean;
  loadingConversations: boolean;
  selectedAgent: AgentRecord | null;
  selectedAgentId: string | null;
  selectedConversationId: string | null;
  onSelectAgent: (agentId: string) => void;
  onSelectConversation: (conversationId: string) => void;
  onCreateConversation: (name: string) => Promise<void>;
}) {
  const [filter, setFilter] = useState("");
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [createPending, setCreatePending] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);

  async function submitCreate() {
    setCreatePending(true);
    setCreateError(null);
    try {
      await onCreateConversation(newName.trim());
      setCreating(false);
      setNewName("");
    } catch (error) {
      setCreateError(error instanceof Error ? error.message : String(error));
    } finally {
      setCreatePending(false);
    }
  }
  const normalizedFilter = filter.trim().toLowerCase();
  // Sampled once per render for relative timestamps; the list re-renders often
  // enough (selection, filter, count arrivals) to keep "2h" style labels fresh.
  const nowMs = Date.now();

  const filteredAgents = useMemo(() => {
    if (!normalizedFilter) {
      return agents;
    }
    return agents.filter((agent) =>
      matchesNavFilter(agent.name, agent.slug, agent.id, normalizedFilter),
    );
  }, [agents, normalizedFilter]);

  const filteredConversations = useMemo(() => {
    if (!normalizedFilter) {
      return conversations;
    }
    return conversations.filter((conversation) =>
      matchesNavFilter(
        conversation.record.name,
        conversation.record.slug,
        conversation.record.id,
        normalizedFilter,
      ),
    );
  }, [conversations, normalizedFilter]);

  return (
    <nav className="left-nav" aria-label="Agents and conversations">
      <label className="nav-filter">
        <span className="sr-only">Filter agents and conversations</span>
        <input
          onChange={(event) => setFilter(event.target.value)}
          placeholder="Filter…"
          spellCheck={false}
          type="search"
          value={filter}
        />
      </label>
      <section className="nav-section">
        <div className="section-header">
          <h2>Agents</h2>
          <span>{loadingAgents ? "…" : filteredAgents.length}</span>
        </div>
        <div className="nav-list">
          {filteredAgents.map((agent) => (
            <button
              className={`nav-item ${agent.id === selectedAgentId ? "selected" : ""}`}
              key={agent.id}
              onClick={() => onSelectAgent(agent.id)}
              type="button"
            >
              <span>{agent.name || agent.slug || shortId(agent.id)}</span>
              <code>{agent.slug || shortId(agent.id)}</code>
            </button>
          ))}
          {!loadingAgents && filteredAgents.length === 0 ? (
            <div className="empty-inline">
              {normalizedFilter ? "No matches." : "No agents."}
            </div>
          ) : null}
          {loadingAgents ? (
            <SkeletonRows className="nav-skeleton" count={3} />
          ) : null}
        </div>
      </section>

      <section className="nav-section nav-section-conversations">
        <div className="section-header">
          <h2>Conversations</h2>
          <div className="section-header-meta">
            <span>
              {loadingConversations ? "…" : filteredConversations.length}
            </span>
            {selectedAgent ? (
              <button
                aria-expanded={creating}
                aria-label="New conversation"
                className="ghost-button new-conversation-toggle"
                onClick={() => {
                  setCreateError(null);
                  setCreating((value) => !value);
                }}
                type="button"
              >
                + New
              </button>
            ) : null}
          </div>
        </div>
        {selectedAgent ? (
          <div className="selected-context">
            <span>{selectedAgent.name || selectedAgent.slug}</span>
            <code>{shortId(selectedAgent.id)}</code>
          </div>
        ) : null}
        {creating && selectedAgent ? (
          <form
            className="new-conversation-form"
            onSubmit={(event) => {
              event.preventDefault();
              void submitCreate();
            }}
          >
            <input
              autoFocus
              className="new-conversation-input"
              disabled={createPending}
              onChange={(event) => setNewName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Escape") {
                  setCreating(false);
                  setCreateError(null);
                }
              }}
              placeholder="Name (optional)"
              value={newName}
            />
            <div className="new-conversation-actions">
              <button
                className="primary-button"
                disabled={createPending}
                type="submit"
              >
                {createPending ? "creating…" : "Create"}
              </button>
              <button
                className="ghost-button"
                disabled={createPending}
                onClick={() => {
                  setCreating(false);
                  setCreateError(null);
                }}
                type="button"
              >
                Cancel
              </button>
            </div>
            {createError ? (
              <div className="new-conversation-error">{createError}</div>
            ) : null}
          </form>
        ) : null}
        <div className="nav-list">
          {filteredConversations.map((conversation) => {
            const lastActiveMs = eventIdToTimestamp(
              conversation.record.latest_event_id,
            );
            const turns = turnCounts.get(
              `${selectedAgentId}:${conversation.record.id}`,
            );
            return (
              <button
                className={`nav-item conversation-nav-item ${
                  conversation.record.id === selectedConversationId
                    ? "selected"
                    : ""
                }`}
                key={conversation.record.id}
                onClick={() => onSelectConversation(conversation.record.id)}
                style={
                  {
                    "--conv-color": conversationColor(conversation.record.id),
                  } as CSSProperties
                }
                type="button"
              >
                <span className="conversation-nav-name">
                  <span aria-hidden="true" className="conversation-dot" />
                  {conversation.record.name ||
                    conversation.record.slug ||
                    shortId(conversation.record.id)}
                </span>
                <span className="conversation-nav-meta">
                  <code>
                    {conversation.record.slug ||
                      shortId(conversation.record.id)}
                  </code>
                  {lastActiveMs != null ? (
                    <time
                      className="conversation-nav-stat"
                      dateTime={new Date(lastActiveMs).toISOString()}
                    >
                      {formatRelativeTime(lastActiveMs, nowMs)}
                    </time>
                  ) : null}
                  {turns != null && turns > 0 ? (
                    <span className="conversation-nav-stat">
                      {turns} {turns === 1 ? "turn" : "turns"}
                    </span>
                  ) : null}
                </span>
              </button>
            );
          })}
          {!loadingConversations &&
          selectedAgentId &&
          filteredConversations.length === 0 ? (
            <div className="empty-inline">
              {normalizedFilter ? "No matches." : "No conversations."}
            </div>
          ) : null}
          {!selectedAgentId ? (
            <div className="empty-inline">Select an agent.</div>
          ) : null}
          {loadingConversations ? (
            <SkeletonRows className="nav-skeleton" count={4} />
          ) : null}
        </div>
      </section>
    </nav>
  );
}

function matchesNavFilter(
  name: string | null | undefined,
  slug: string | null | undefined,
  id: string,
  filter: string,
): boolean {
  const haystack = [name, slug, id].filter(Boolean).join(" ").toLowerCase();
  return haystack.includes(filter);
}
