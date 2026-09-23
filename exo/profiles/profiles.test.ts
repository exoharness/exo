import { HarnessToolRegistry, type TurnContext } from "@exo/harness";
import { describe, expect, it } from "vitest";

import { resolveExoProfile } from "./index";

describe("Exo profiles", () => {
  it("defaults to the practical profile", () => {
    expect(resolveExoProfile(undefined).name).toBe("practical");
  });

  it("gives bootstrap exactly the recovery capabilities", () => {
    const profile = resolveExoProfile("bootstrap");
    const context = {
      agentConfig: { enableAgentToolCreation: false },
    } as TurnContext;
    const builtInNames = profile.builtInToolNames(context);
    expect(builtInNames).toEqual(["shell", "inspect_tools", "manage_tool"]);

    const tools = new HarnessToolRegistry(context);
    profile.registerTools(tools, context);
    expect([
      ...builtInNames,
      ...tools.definitions().map(({ name }) => name),
    ]).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
      "rebuild_and_restart_exo",
    ]);
  });

  it("keeps practical extensions classified as library tools", () => {
    const profile = resolveExoProfile("practical");
    const context = {
      agentConfig: { enableAgentToolCreation: false },
    } as TurnContext;
    const tools = new HarnessToolRegistry(context);
    expect(profile.builtInToolNames(context)).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
    ]);
    profile.registerTools(tools, context);

    expect(tools.get("create_adapter")?.source).toBe("library");
    expect(tools.get("snapshot_sandbox")?.source).toBe("library");
    expect(tools.get("web_search")?.source).toBe("library");
    expect(tools.get("rebuild_and_restart_exo")?.source).toBe("built_in");
    expect([
      ...profile.builtInToolNames(context),
      ...tools
        .instances()
        .filter(({ source }) => source === "built_in")
        .map(({ definition }) => definition.name),
    ]).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
      "rebuild_and_restart_exo",
    ]);
  });

  it("exposes legacy agent-tool creation only when enabled", () => {
    const profile = resolveExoProfile("practical");
    const context = {
      agentConfig: { enableAgentToolCreation: true },
    } as TurnContext;

    expect(profile.builtInToolNames(context)).toEqual([
      "shell",
      "inspect_tools",
      "manage_tool",
      "install_agent_tool",
      "uninstall_agent_tool",
    ]);
  });

  it("rejects unknown profiles", () => {
    expect(() => resolveExoProfile("unknown")).toThrow(
      "expected one of bootstrap, practical, memory-only",
    );
  });

  it("memory-only keeps memory and skill use but no self-modification", () => {
    const profile = resolveExoProfile("memory-only");
    expect(profile.selfModification).toBe(false);
    const context = {
      agentConfig: { enableAgentToolCreation: true },
    } as TurnContext;
    expect(profile.builtInToolNames(context)).toEqual([
      "shell",
      "inspect_tools",
    ]);

    const tools = new HarnessToolRegistry(context);
    profile.registerTools(tools, context);
    const names = tools.definitions().map(({ name }) => name);
    for (const kept of [
      "remember",
      "forget",
      "list_skills",
      "use_skill",
      "read_skill_file",
      "todowrite",
      "web_search",
    ]) {
      expect(names).toContain(kept);
    }
    for (const removed of [
      "install_skill",
      "uninstall_skill",
      "rebuild_and_restart_exo",
      "manage_tool",
      "install_agent_tool",
    ]) {
      expect(names).not.toContain(removed);
    }
    expect(tools.get("use_skill")?.source).toBe("library");
  });

  it("existing profiles allow self-modification", () => {
    expect(resolveExoProfile("practical").selfModification).toBe(true);
    expect(resolveExoProfile("bootstrap").selfModification).toBe(true);
  });
});
