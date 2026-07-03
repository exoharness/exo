import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
} from "react";
import { createPortal } from "react-dom";
import type {
  AgentRecord,
  ConversationHandleInfo,
  Event,
} from "../api/protocol";
import { copyText } from "../lib/copy";
import {
  buildPaletteCommands,
  clampActiveIndex,
  filterPaletteCommands,
  GROUP_ORDER,
  sanitizeFilename,
  wrapActiveIndex,
} from "../lib/commandPalette";
import {
  downloadConversationJson,
  downloadConversationMarkdown,
} from "../lib/exportConversation";

interface CommandPaletteProps {
  open: boolean;
  onClose: () => void;
  theme: "light" | "dark";
  agents: AgentRecord[];
  conversations: ConversationHandleInfo[];
  selectedAgentId: string | null;
  selectedConversationId: string | null;
  selectedConversation: ConversationHandleInfo | null;
  events: Event[];
  onSelectAgent: (agentId: string) => void;
  onSelectConversation: (conversationId: string) => void;
  onToggleTheme: () => void;
  onScrollToLatest: () => void;
}

export function CommandPalette({
  open,
  onClose,
  theme,
  agents,
  conversations,
  selectedAgentId,
  selectedConversationId,
  selectedConversation,
  events,
  onSelectAgent,
  onSelectConversation,
  onToggleTheme,
  onScrollToLatest,
}: CommandPaletteProps) {
  const [query, setQuery] = useState("");
  const [activeIndex, setActiveIndex] = useState(0);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);

  const exportStem = useMemo(
    () =>
      sanitizeFilename(
        selectedConversation?.record.slug ||
          selectedConversation?.record.name ||
          "conversation",
      ),
    [selectedConversation],
  );

  const commands = useMemo(
    () =>
      buildPaletteCommands({
        theme,
        agents,
        conversations,
        selectedAgentId,
        selectedConversationId,
        events,
        exportStem,
        onClose,
        onScrollToLatest,
        onToggleTheme,
        onSelectAgent,
        onSelectConversation,
        copyConversationId: (conversationId) => {
          void copyText(conversationId);
        },
        exportJson: downloadConversationJson,
        exportMarkdown: downloadConversationMarkdown,
      }),
    [
      agents,
      conversations,
      events,
      exportStem,
      onClose,
      onScrollToLatest,
      onSelectAgent,
      onSelectConversation,
      onToggleTheme,
      selectedAgentId,
      selectedConversationId,
      theme,
    ],
  );

  const filtered = useMemo(
    () => filterPaletteCommands(commands, query),
    [commands, query],
  );

  useEffect(() => {
    if (open) {
      setQuery("");
      setActiveIndex(0);
      const raf = requestAnimationFrame(() => inputRef.current?.focus());
      return () => cancelAnimationFrame(raf);
    }
  }, [open]);

  useEffect(() => {
    setActiveIndex((current) => clampActiveIndex(current, filtered.length));
  }, [filtered.length]);

  useEffect(() => {
    if (!open) {
      return;
    }
    const node = listRef.current?.querySelector<HTMLElement>(
      `[data-cmd-index="${activeIndex}"]`,
    );
    node?.scrollIntoView({ block: "nearest" });
  }, [activeIndex, open]);

  if (!open) {
    return null;
  }

  function moveActive(delta: number) {
    if (filtered.length === 0) {
      return;
    }
    setActiveIndex((current) =>
      wrapActiveIndex(current, delta, filtered.length),
    );
  }

  function handleKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      moveActive(1);
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      moveActive(-1);
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      const command = filtered[activeIndex];
      if (command && !command.disabled) {
        command.run();
      }
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      onClose();
    }
  }

  let flatIndex = -1;

  return createPortal(
    <div
      className="command-palette-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div
        className="command-palette"
        role="dialog"
        aria-modal="true"
        aria-label="Command palette"
        onKeyDown={handleKeyDown}
      >
        <div className="command-palette-input-row">
          <PaletteSearchIcon />
          <input
            ref={inputRef}
            aria-label="Search commands"
            className="command-palette-input"
            onChange={(event) => {
              setQuery(event.target.value);
              setActiveIndex(0);
            }}
            placeholder="Search commands…"
            spellCheck={false}
            type="text"
            value={query}
          />
          <kbd className="command-palette-kbd">esc</kbd>
        </div>
        <div className="command-palette-list" ref={listRef} role="listbox">
          {filtered.length === 0 ? (
            <div className="command-empty">No matching commands.</div>
          ) : (
            GROUP_ORDER.map((group) => {
              const groupItems = filtered.filter(
                (command) => command.group === group,
              );
              if (groupItems.length === 0) {
                return null;
              }
              return (
                <div className="command-group" key={group}>
                  <div className="command-group-header">{group}</div>
                  {groupItems.map((command) => {
                    flatIndex += 1;
                    const index = flatIndex;
                    const isActive = index === activeIndex;
                    return (
                      <button
                        aria-selected={isActive}
                        className={`command-item ${isActive ? "is-active" : ""}`}
                        data-cmd-index={index}
                        disabled={command.disabled}
                        key={command.id}
                        onClick={command.run}
                        onMouseMove={() => setActiveIndex(index)}
                        role="option"
                        type="button"
                      >
                        <span className="command-item-label">
                          {command.label}
                        </span>
                        {command.hint ? (
                          <span className="command-item-hint">
                            {command.hint}
                          </span>
                        ) : null}
                      </button>
                    );
                  })}
                </div>
              );
            })
          )}
        </div>
      </div>
    </div>,
    document.body,
  );
}

function PaletteSearchIcon() {
  return (
    <svg
      aria-hidden="true"
      className="command-palette-icon"
      focusable="false"
      viewBox="0 0 16 16"
    >
      <circle cx="7" cy="7" r="4.2" />
      <path d="m10.2 10.2 3 3" />
    </svg>
  );
}
