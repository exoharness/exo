import type { Event } from "../api/protocol";
import { formatJson, formatTime, renderMessageContent } from "./rendering";

export function downloadConversationJson(
  events: Event[],
  filenameStem: string,
) {
  const blob = new Blob([JSON.stringify(events, null, 2)], {
    type: "application/json;charset=utf-8",
  });
  triggerDownload(blob, `${filenameStem}.json`);
}

export function downloadConversationMarkdown(
  events: Event[],
  filenameStem: string,
) {
  const markdown = buildConversationMarkdown(events);
  const blob = new Blob([markdown], { type: "text/markdown;charset=utf-8" });
  triggerDownload(blob, `${filenameStem}.md`);
}

export function buildConversationMarkdown(events: Event[]): string {
  const lines: string[] = ["# Conversation export", ""];
  const ordered = [...events].sort((left, right) => {
    const timeCompare = left.created_at.localeCompare(right.created_at);
    return timeCompare === 0 ? left.id.localeCompare(right.id) : timeCompare;
  });

  for (const event of ordered) {
    const data = event.data;
    if (data.type === "messages") {
      for (const message of data.messages) {
        const role =
          typeof message.role === "string" ? message.role : "message";
        if (role !== "user" && role !== "assistant") {
          continue;
        }
        lines.push(`## ${role} · ${formatTime(event.created_at)}`, "");
        lines.push(renderMessageContent(message), "");
      }
      continue;
    }

    if (data.type === "tool_requested") {
      lines.push(
        `## tool · ${data.request.function_name} · ${formatTime(event.created_at)}`,
        "",
      );
      lines.push("```json", formatJson(data.request.arguments), "```", "");
      continue;
    }

    if (data.type === "tool_result") {
      lines.push(`## tool result · ${formatTime(event.created_at)}`, "");
      lines.push("```json", formatJson(data.result), "```", "");
    }
  }

  return lines.join("\n").trimEnd() + "\n";
}

function triggerDownload(blob: Blob, filename: string) {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  anchor.rel = "noopener";
  anchor.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 0);
}
