import { describe, expect, it, vi } from "vitest";
import type { AgentRecord, ConversationHandleInfo } from "../api/protocol";
import { makeEvent } from "../test/fixtures";
import {
  buildPaletteCommands,
  clampActiveIndex,
  filterPaletteCommands,
  fuzzyScore,
  sanitizeFilename,
  wrapActiveIndex,
} from "./commandPalette";

const agentA: AgentRecord = {
  id: "agent-alpha-id-0001",
  slug: "alpha",
  name: "Alpha Agent",
};

const agentB: AgentRecord = {
  id: "agent-beta-id-00002",
  slug: "beta",
  name: "Beta Bot",
};

const conversationA: ConversationHandleInfo = {
  agent_id: agentA.id,
  record: {
    id: "conv-daily-standup-001",
    slug: "daily-standup",
    name: "Daily Standup",
    latest_event_id: "evt-latest-aaa",
  },
};

const conversationB: ConversationHandleInfo = {
  agent_id: agentB.id,
  record: {
    id: "conv-debug-session-002",
    slug: "debug",
    name: "Debug Session",
    latest_event_id: null,
  },
};

function makeCommands(
  overrides: Partial<Parameters<typeof buildPaletteCommands>[0]> = {},
) {
  return buildPaletteCommands({
    theme: "dark",
    agents: [agentA, agentB],
    conversations: [conversationA, conversationB],
    selectedAgentId: agentA.id,
    selectedConversationId: conversationA.record.id,
    events: [makeEvent({ type: "turn_started" })],
    exportStem: "daily-standup",
    onClose: vi.fn(),
    onScrollToLatest: vi.fn(),
    onToggleTheme: vi.fn(),
    onSelectAgent: vi.fn(),
    onSelectConversation: vi.fn(),
    copyConversationId: vi.fn(),
    exportJson: vi.fn(),
    exportMarkdown: vi.fn(),
    ...overrides,
  });
}

describe("fuzzyScore", () => {
  it("returns null when query characters are not a subsequence", () => {
    expect(fuzzyScore("xyz", "jump to latest message")).toBeNull();
    expect(fuzzyScore("themez", "toggle theme light dark")).toBeNull();
  });

  it("prefers earlier and consecutive matches (lower score is better)", () => {
    const jumpScore = fuzzyScore("jmp", "Jump to latest message jump");
    const themeAtEnd = fuzzyScore("thm", "Switch to light theme");
    const themeAtStart = fuzzyScore("thm", "theme toggle light dark");
    expect(jumpScore).not.toBeNull();
    expect(themeAtStart).not.toBeNull();
    expect(themeAtEnd).not.toBeNull();
    expect(themeAtStart!).toBeLessThan(themeAtEnd!);
  });

  it("is case-insensitive", () => {
    expect(
      fuzzyScore("JSON", "Export current conversation (json)"),
    ).not.toBeNull();
    expect(fuzzyScore("json", "Export current conversation (json)")).toBe(
      fuzzyScore("JSON", "Export current conversation (json)"),
    );
  });

  it("rewards consecutive character runs", () => {
    const scattered = fuzzyScore("jmp", "jump to latest message");
    const prefix = fuzzyScore("jmp", "jmp message");
    expect(scattered).not.toBeNull();
    expect(prefix).not.toBeNull();
    expect(prefix!).toBeLessThan(scattered!);
  });
});

describe("filterPaletteCommands", () => {
  it("returns all commands in group order when query is empty", () => {
    const commands = makeCommands();
    const filtered = filterPaletteCommands(commands, "   ");
    const groups = filtered.map((command) => command.group);
    expect(groups.indexOf("Actions")).toBeLessThan(groups.indexOf("Agents"));
    expect(groups.indexOf("Agents")).toBeLessThan(
      groups.indexOf("Conversations"),
    );
    expect(filtered.length).toBe(commands.length);
  });

  it("drops non-matching commands and ranks better matches first within each group", () => {
    const commands = makeCommands();
    const filtered = filterPaletteCommands(commands, "theme");
    const labels = filtered.map((command) => command.label);
    expect(
      labels.every(
        (label) =>
          label.toLowerCase().includes("theme") ||
          label.toLowerCase().includes("switch"),
      ),
    ).toBe(true);
    expect(labels[0]).toContain("theme");
    expect(filtered.every((command) => command.group === "Actions")).toBe(true);
  });

  it("matches against keywords as well as labels", () => {
    const commands = makeCommands();
    const filtered = filterPaletteCommands(commands, "standup");
    expect(
      filtered.some(
        (command) => command.id === "conversation:conv-daily-standup-001",
      ),
    ).toBe(true);
  });

  it("returns an empty list when nothing matches", () => {
    expect(filterPaletteCommands(makeCommands(), "zzzznotfound")).toEqual([]);
  });
});

describe("buildPaletteCommands", () => {
  it("disables jump/export actions without selection or events", () => {
    const commands = makeCommands({
      selectedConversationId: null,
      events: [],
    });
    const byId = Object.fromEntries(
      commands.map((command) => [command.id, command]),
    );
    expect(byId["action:jump-latest"]?.disabled).toBe(true);
    expect(byId["action:copy-conversation-id"]?.disabled).toBe(true);
    expect(byId["action:export-json"]?.disabled).toBe(true);
    expect(byId["action:export-md"]?.disabled).toBe(true);
    expect(byId["action:toggle-theme"]?.disabled).toBeUndefined();
  });

  it("marks the selected agent and conversation with current hints", () => {
    const commands = makeCommands();
    const agent = commands.find(
      (command) => command.id === `agent:${agentA.id}`,
    );
    const conversation = commands.find(
      (command) => command.id === `conversation:${conversationA.record.id}`,
    );
    expect(agent?.hint).toBe("current");
    expect(conversation?.hint).toBe("current");
  });

  it("labels unselected conversations with their owning agent", () => {
    const commands = makeCommands();
    const conversation = commands.find(
      (command) => command.id === `conversation:${conversationB.record.id}`,
    );
    expect(conversation?.hint).toBe("in Beta Bot");
  });

  it("closes the palette after running an action", () => {
    const onClose = vi.fn();
    const onToggleTheme = vi.fn();
    const commands = makeCommands({ onClose, onToggleTheme });
    const toggle = commands.find(
      (command) => command.id === "action:toggle-theme",
    );
    toggle?.run();
    expect(onToggleTheme).toHaveBeenCalledOnce();
    expect(onClose).toHaveBeenCalledOnce();
  });
});

describe("sanitizeFilename", () => {
  it("lowercases and replaces unsafe characters", () => {
    expect(sanitizeFilename(" Daily Standup! ")).toBe("daily-standup-");
    expect(sanitizeFilename("foo/bar:baz")).toBe("foo-bar-baz");
  });

  it('falls back to "conversation" when nothing remains', () => {
    expect(sanitizeFilename("   !!!   ")).toBe("-");
    expect(sanitizeFilename("")).toBe("conversation");
  });
});

describe("keyboard index helpers", () => {
  it("wraps active index around list bounds", () => {
    expect(wrapActiveIndex(0, -1, 5)).toBe(4);
    expect(wrapActiveIndex(4, 1, 5)).toBe(0);
    expect(wrapActiveIndex(2, 0, 0)).toBe(0);
  });

  it("clamps active index when the filtered list shrinks", () => {
    expect(clampActiveIndex(4, 3)).toBe(2);
    expect(clampActiveIndex(1, 0)).toBe(0);
  });
});
