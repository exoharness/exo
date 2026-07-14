import type { EmulatorClient, FramePayload } from "./emulator-client";

export interface ToolResult {
  text: string;
  // When set, the harness appends this screenshot to the model input so the
  // agent sees the consequence of its action immediately.
  frame?: FramePayload;
  // Console marker for the demo narration, e.g. "PLAYBOOK" or "NEW TOOL: x".
  improvement?: string;
}

export interface AgentTool {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  execute(args: Record<string, unknown>): Promise<ToolResult>;
  // Agent-authored tools usually drive the emulator but return plain text;
  // when true the harness fetches a fresh frame after each call so the model
  // still sees the resulting screen.
  attachFrameAfter?: boolean;
}

// Injected into agent-authored tool modules so they can compose the
// emulator primitives without knowing about HTTP.
export interface AgentToolContext {
  emulator: EmulatorClient;
  log(message: string): void;
}
