import type {
  BuiltInToolName,
  HarnessToolRegistry,
  TurnContext,
} from "@exo/harness";

export type ExoProfileName = "bootstrap" | "practical" | "memory-only";

export interface ExoProfile {
  name: ExoProfileName;
  // Whether the agent may change its own policy: install tools or skills and
  // rebuild itself. Prompt sections about those abilities follow this flag.
  selfModification: boolean;
  builtInToolNames(context: TurnContext): BuiltInToolName[];
  registerTools(
    tools: HarnessToolRegistry,
    context: TurnContext,
  ): Promise<void> | void;
}
