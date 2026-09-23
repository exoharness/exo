import {
  createSkillToolInstances,
  HarnessToolRegistry,
  registerAdapterTools,
} from "@exo/harness";
import { registerIntrospectionTools } from "../tools/introspection-tools";
import { registerMemoryTools } from "../tools/memory-tools";
import { registerSandboxTools } from "../tools/sandbox-tools";
import { registerSchedulerTools } from "../tools/scheduler-tools";
import { registerTodoTools } from "../tools/todo-tools";
import { registerWebTools } from "../tools/web-tools";
import type { ExoProfile } from "./types";

// Skill tools that change the installed set; the rest read and use skills.
const SKILL_MUTATION_TOOLS = new Set(["install_skill", "uninstall_skill"]);

// The practical profile minus every way to change Exo's own policy: no tool
// installation, no skill installation, no rebuild. Memory stays writable, and
// already-installed skills stay usable, so a run can learn facts but not
// capabilities. Meant as a control arm for self-improvement evaluations.
export const memoryOnlyProfile: ExoProfile = {
  name: "memory-only",
  selfModification: false,
  builtInToolNames() {
    return ["shell", "inspect_tools"];
  },
  registerTools(tools, context) {
    const libraryTools = new HarnessToolRegistry(context);
    registerSchedulerTools(libraryTools);
    registerAdapterTools(libraryTools);
    registerIntrospectionTools(libraryTools);
    registerSandboxTools(libraryTools);
    registerMemoryTools(libraryTools);
    registerTodoTools(libraryTools);
    for (const tool of createSkillToolInstances()) {
      if (!SKILL_MUTATION_TOOLS.has(tool.definition.name)) {
        libraryTools.register(tool);
      }
    }
    registerWebTools(libraryTools);
    for (const tool of libraryTools.instances()) {
      tools.register({ ...tool, source: "library" });
    }
  },
};
