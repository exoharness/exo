import { describe, expect, it } from "vitest";

import {
  createToolRegistry,
  type AgentConfig,
  type ConversationConfig,
  type TurnContext,
} from "@exo/harness";

import {
  competentCodingInstructions,
  registerCompetentCodingTools,
} from "./competent-coding-agent-harness";

function fakeContext(): TurnContext {
  const agentConfig: AgentConfig = {
    instructions: [],
    harness: "typescript",
    typescript: {
      modulePath: "examples/typescript/competent-coding-agent-harness.ts",
      toolModulePaths: [],
    },
    enableAgentToolCreation: false,
    sandbox: {
      provider: "local_process",
      mounts: [],
      enableNetworking: false,
      scope: "conversation",
    },
    model: "test-model",
  };
  const conversationConfig: ConversationConfig = {
    shellProgram: "/bin/bash",
    mounts: [
      {
        hostPath: "/host/project",
        mountPath: "/workspace",
        mode: "rw",
      },
    ],
  };
  const emptyArtifacts = {
    async listArtifacts() {
      return [];
    },
  };

  return {
    agentConfig,
    conversationConfig,
    request: { input: [] },
    streaming: false,
    exoharness: {
      current: {
        agent: emptyArtifacts,
        conversation: emptyArtifacts,
      },
    },
    async executeTool() {
      return {
        stdout:
          "Working directory: /workspace\n" +
          "Git worktree root: /workspace\n" +
          "<root_agents_md>\nRun pnpm test before finishing.\n</root_agents_md>\n",
        stderr: "",
        exit_code: 0,
      };
    },
  } as unknown as TurnContext;
}

describe("competent coding agent harness", () => {
  it("registers the core coding, planning, memory, and skill tools", async () => {
    const context = fakeContext();
    const tools = createToolRegistry(context);

    await registerCompetentCodingTools(tools, context);

    expect(
      tools
        .definitions()
        .map((tool) => tool.name)
        .sort(),
    ).toEqual(
      [
        "forget",
        "install_skill",
        "list_skills",
        "read_skill_file",
        "remember",
        "shell",
        "todowrite",
        "uninstall_skill",
        "use_skill",
      ].sort(),
    );
  });

  it("injects live workspace details and root repository instructions", async () => {
    const instructions = await competentCodingInstructions(fakeContext());
    const content = instructions.map((message) => String(message.content));

    expect(instructions[0].role).toBe("system");
    expect(content.join("\n")).toContain("Working directory: /workspace");
    expect(content.join("\n")).toContain("Run pnpm test before finishing.");
    expect(content.join("\n")).toContain(
      "/workspace (rw, host path /host/project)",
    );
  });
});
