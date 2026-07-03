// @vitest-environment jsdom
import "../test/setup.ts";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentRecord, ConversationHandleInfo } from "../api/protocol";
import { makeEvent } from "../test/fixtures";
import { CommandPalette } from "./CommandPalette";

const agent: AgentRecord = {
  id: "agent-1",
  slug: "helper",
  name: "Helper",
};

const conversation: ConversationHandleInfo = {
  agent_id: agent.id,
  record: {
    id: "conv-1",
    slug: "main",
    name: "Main Thread",
    latest_event_id: "evt-9",
  },
};

function renderPalette(
  overrides: Partial<Parameters<typeof CommandPalette>[0]> = {},
) {
  const props = {
    open: true,
    onClose: vi.fn(),
    theme: "dark" as const,
    agents: [agent],
    conversations: [conversation],
    selectedAgentId: agent.id,
    selectedConversationId: conversation.record.id,
    selectedConversation: conversation,
    events: [
      makeEvent({
        type: "messages",
        messages: [{ role: "user", content: "hello" }],
        response_id: null,
      }),
    ],
    onSelectAgent: vi.fn(),
    onSelectConversation: vi.fn(),
    onToggleTheme: vi.fn(),
    onScrollToLatest: vi.fn(),
    ...overrides,
  };
  render(<CommandPalette {...props} />);
  return props;
}

describe("CommandPalette", () => {
  beforeEach(() => {
    Element.prototype.scrollIntoView = vi.fn();
  });

  afterEach(() => {
    cleanup();
  });

  it("filters visible commands as the user types", () => {
    renderPalette();
    const input = screen.getByRole("textbox", { name: "Search commands" });

    fireEvent.change(input, { target: { value: "export json" } });

    const options = screen.getAllByRole("option");
    expect(options).toHaveLength(1);
    expect(options[0]).toHaveTextContent("Export current conversation (json)");
    expect(screen.queryByText("Switch to light theme")).toBeNull();
  });

  it("shows an empty state when the query matches nothing", () => {
    renderPalette();
    fireEvent.change(screen.getByRole("textbox", { name: "Search commands" }), {
      target: { value: "zzzz-no-match" },
    });
    expect(screen.getByText("No matching commands.")).toBeInTheDocument();
  });

  it("moves selection with arrow keys and runs the active command on Enter", () => {
    const onToggleTheme = vi.fn();
    const onClose = vi.fn();
    renderPalette({ onToggleTheme, onClose });
    const dialog = screen.getByRole("dialog", { name: "Command palette" });

    fireEvent.keyDown(dialog, { key: "ArrowDown" });
    fireEvent.keyDown(dialog, { key: "Enter" });

    expect(onToggleTheme).toHaveBeenCalledOnce();
    expect(onClose).toHaveBeenCalledOnce();
  });

  it("wraps keyboard selection from last item back to the first", () => {
    renderPalette();
    const dialog = screen.getByRole("dialog", { name: "Command palette" });
    const options = screen.getAllByRole("option");
    const lastIndex = options.length - 1;

    for (let step = 0; step < lastIndex; step += 1) {
      fireEvent.keyDown(dialog, { key: "ArrowDown" });
    }
    expect(options[lastIndex]).toHaveAttribute("aria-selected", "true");

    fireEvent.keyDown(dialog, { key: "ArrowDown" });
    expect(screen.getAllByRole("option")[0]).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });

  it("does not run disabled commands on Enter", () => {
    const onScrollToLatest = vi.fn();
    renderPalette({
      selectedConversationId: null,
      selectedConversation: null,
      onScrollToLatest,
    });
    const dialog = screen.getByRole("dialog", { name: "Command palette" });
    const jump = screen.getByRole("option", {
      name: /Jump to latest message/i,
    });
    expect(jump).toBeDisabled();

    fireEvent.keyDown(dialog, { key: "Enter" });
    expect(onScrollToLatest).not.toHaveBeenCalled();
  });

  it("closes on Escape and backdrop click", () => {
    const onClose = vi.fn();
    renderPalette({ onClose });
    const dialog = screen.getByRole("dialog", { name: "Command palette" });

    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(onClose).toHaveBeenCalledOnce();

    const backdrop = dialog.parentElement;
    expect(backdrop).not.toBeNull();
    fireEvent.mouseDown(backdrop!, { target: backdrop });
    expect(onClose).toHaveBeenCalledTimes(2);
  });

  it("groups filtered results under section headers", () => {
    renderPalette();
    const listbox = screen.getByRole("listbox");
    expect(within(listbox).getByText("Actions")).toBeInTheDocument();
    expect(within(listbox).getByText("Agents")).toBeInTheDocument();
    expect(within(listbox).getByText("Conversations")).toBeInTheDocument();
  });
});
