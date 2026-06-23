import type {
  AgentRecord,
  ConversationHandleInfo,
  Event,
} from "../api/protocol";
import { shortId } from "./rendering";

export const GROUP_ORDER = ["Actions", "Agents", "Conversations"] as const;

export interface PaletteCommand {
  id: string;
  group: string;
  label: string;
  hint?: string;
  keywords: string;
  disabled?: boolean;
  run: () => void;
}

export interface BuildPaletteCommandsInput {
  theme: "light" | "dark";
  agents: AgentRecord[];
  conversations: ConversationHandleInfo[];
  selectedAgentId: string | null;
  selectedConversationId: string | null;
  events: Event[];
  exportStem: string;
  onClose: () => void;
  onScrollToLatest: () => void;
  onToggleTheme: () => void;
  onSelectAgent: (agentId: string) => void;
  onSelectConversation: (conversationId: string) => void;
  copyConversationId: (conversationId: string) => void;
  exportJson: (events: Event[], stem: string) => void;
  exportMarkdown: (events: Event[], stem: string) => void;
}

export function buildPaletteCommands(
  input: BuildPaletteCommandsInput,
): PaletteCommand[] {
  const {
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
    copyConversationId,
    exportJson,
    exportMarkdown,
  } = input;

  const hasConversation = Boolean(selectedConversationId);
  const hasEvents = events.length > 0;

  const run = (action: () => void): (() => void) => {
    return () => {
      action();
      onClose();
    };
  };

  const actions: PaletteCommand[] = [
    {
      id: "action:jump-latest",
      group: "Actions",
      label: "Jump to latest message",
      hint: "transcript",
      keywords: "jump latest bottom newest scroll message",
      disabled: !hasConversation,
      run: run(onScrollToLatest),
    },
    {
      id: "action:toggle-theme",
      group: "Actions",
      label: `Switch to ${theme === "dark" ? "light" : "dark"} theme`,
      hint: theme === "dark" ? "light" : "dark",
      keywords: "toggle theme light dark mode appearance",
      run: run(onToggleTheme),
    },
    {
      id: "action:copy-conversation-id",
      group: "Actions",
      label: "Copy current conversation id",
      hint: selectedConversationId
        ? shortId(selectedConversationId)
        : undefined,
      keywords: "copy conversation id clipboard",
      disabled: !selectedConversationId,
      run: run(() => {
        if (selectedConversationId) {
          copyConversationId(selectedConversationId);
        }
      }),
    },
    {
      id: "action:export-json",
      group: "Actions",
      label: "Export current conversation (json)",
      hint: "download",
      keywords: "export download json conversation save",
      disabled: !hasEvents,
      run: run(() => exportJson(events, exportStem)),
    },
    {
      id: "action:export-md",
      group: "Actions",
      label: "Export current conversation (md)",
      hint: "download",
      keywords: "export download markdown md conversation save",
      disabled: !hasEvents,
      run: run(() => exportMarkdown(events, exportStem)),
    },
  ];

  const agentCommands: PaletteCommand[] = agents.map((agent) => {
    const name = agent.name || agent.slug || shortId(agent.id);
    return {
      id: `agent:${agent.id}`,
      group: "Agents",
      label: `Switch to agent ${name}`,
      hint:
        agent.id === selectedAgentId
          ? "current"
          : agent.slug || shortId(agent.id),
      keywords: `agent ${agent.name} ${agent.slug} ${agent.id}`,
      run: run(() => onSelectAgent(agent.id)),
    };
  });

  const conversationCommands: PaletteCommand[] = conversations.map(
    (conversation) => {
      const record = conversation.record;
      const name = record.name || record.slug || shortId(record.id);
      const agentName = agents.find(
        (agent) => agent.id === conversation.agent_id,
      );
      const agentLabel = agentName
        ? agentName.name || agentName.slug || shortId(agentName.id)
        : shortId(conversation.agent_id);
      return {
        id: `conversation:${record.id}`,
        group: "Conversations",
        label: `Switch to conversation ${name}`,
        hint:
          record.id === selectedConversationId ? "current" : `in ${agentLabel}`,
        keywords: `conversation ${record.name} ${record.slug} ${record.id} ${agentLabel}`,
        run: run(() => onSelectConversation(record.id)),
      };
    },
  );

  return [...actions, ...agentCommands, ...conversationCommands];
}

export function filterPaletteCommands(
  commands: PaletteCommand[],
  query: string,
): PaletteCommand[] {
  const needle = query.trim();
  const scored = commands
    .map((command) => ({
      command,
      score:
        needle.length === 0
          ? 0
          : fuzzyScore(needle, `${command.label} ${command.keywords}`),
    }))
    .filter(
      (entry): entry is { command: PaletteCommand; score: number } =>
        entry.score !== null,
    );

  const ordered: PaletteCommand[] = [];
  for (const group of GROUP_ORDER) {
    const inGroup = scored.filter((entry) => entry.command.group === group);
    if (needle.length > 0) {
      inGroup.sort((left, right) => left.score - right.score);
    }
    ordered.push(...inGroup.map((entry) => entry.command));
  }
  return ordered;
}

export function fuzzyScore(query: string, text: string): number | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let qi = 0;
  let score = 0;
  let lastMatch = -2;
  for (let ti = 0; ti < t.length && qi < q.length; ti += 1) {
    if (t[ti] === q[qi]) {
      score += ti;
      if (lastMatch === ti - 1) {
        score -= 3;
      }
      lastMatch = ti;
      qi += 1;
    }
  }
  return qi === q.length ? score : null;
}

export function sanitizeFilename(value: string): string {
  const trimmed = value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-");
  return trimmed || "conversation";
}

export function wrapActiveIndex(
  current: number,
  delta: number,
  length: number,
): number {
  if (length === 0) {
    return 0;
  }
  return (current + delta + length) % length;
}

export function clampActiveIndex(current: number, length: number): number {
  if (length === 0) {
    return 0;
  }
  return Math.min(current, length - 1);
}
