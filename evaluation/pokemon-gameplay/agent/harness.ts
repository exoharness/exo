// exo TypeScript harness for the self-improving Pokemon agent.
//
// The exo CLI owns the turn loop, conversation history, and event log; this
// module owns the game semantics and the self-improvement surface. Register
// the agent with:
//
//   exo --harness typescript agent create "Pokemon" \
//     --module evaluation/pokemon-gameplay/agent/harness.ts \
//     --model gpt-5.5 --max-tool-round-trips 20
//
// The emulator sidecar (emulator/server.py) must already be running — see
// ../run.sh. Env: POKEMON_EMULATOR_URL (default http://127.0.0.1:8777),
// POKEMON_RUNTIME_DIR (default ../runtime).
//
// Two hooks do all the work:
// - `instructions` re-runs before every model round: it re-reads the
//   playbook / todos / memory / skills the agent may have just edited,
//   observes RAM for objective milestones (auto-checkpointing each one), and
//   injects the live screen as an image. Screens are never accumulated in
//   history — the model always sees the current frame.
// - `registerTools` rebuilds the registry every round, so a tool the agent
//   installs with install_tool is callable on its very next round trip.
//
// Self-improvement state (playbook, memories, todos, tools, skills) stays in
// the runtime directory on disk, exactly as before — resumable across runs
// and inspectable with the live viewer.

import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  defineHarness,
  type HarnessToolRegistry,
  type JsonValue,
  type Message,
  type ToolInstance,
  type TurnContext,
} from "@exo/harness";

import { runResponsesHarnessTurn } from "../../../examples/typescript/turn-loop";
import {
  buildInstructionMessages,
  samePositionDirective,
  stuckDirective,
} from "./context";
import { EmulatorClient, type FramePayload } from "./emulator-client";
import { EventLog, ProgressTracker, ScreenshotWriter } from "./events";
import { gameTools } from "./game-tools";
import { SelfStore, selfTools } from "./self-tools";
import { SkillsStore, skillTools } from "./skills";
import type { AgentTool } from "./tool-types";

const BASE_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const RUNTIME_DIR =
  process.env.POKEMON_RUNTIME_DIR !== undefined &&
  process.env.POKEMON_RUNTIME_DIR.length > 0
    ? path.resolve(process.env.POKEMON_RUNTIME_DIR)
    : path.join(BASE_DIR, "runtime");
const SYSTEM_PROMPT_PATH = path.join(BASE_DIR, "prompts", "system.md");
const SEED_PLAYBOOK_PATH = path.join(BASE_DIR, "prompts", "playbook.seed.md");

const STUCK_ROUNDS_BEFORE_NUDGE = 6;
const SAME_POSITION_ROUNDS_BEFORE_NUDGE = 6;

const emulator = new EmulatorClient(
  process.env.POKEMON_EMULATOR_URL ?? "http://127.0.0.1:8777",
);

// Lazily initialized once per runner process; all durable state is on disk,
// so a fresh process resumes where the last one stopped.
interface Runtime {
  store: SelfStore;
  skills: SkillsStore;
  events: EventLog;
  progress: ProgressTracker;
  screenshots: ScreenshotWriter;
}

let runtimePromise: Promise<Runtime> | null = null;

function runtime(): Promise<Runtime> {
  runtimePromise ??= (async () => {
    await fs.mkdir(RUNTIME_DIR, { recursive: true });
    const store = new SelfStore(RUNTIME_DIR);
    await store.init(await fs.readFile(SEED_PLAYBOOK_PATH, "utf8"));
    const skills = new SkillsStore(RUNTIME_DIR);
    await skills.init();
    return {
      store,
      skills,
      events: new EventLog(RUNTIME_DIR),
      progress: new ProgressTracker(RUNTIME_DIR),
      screenshots: new ScreenshotWriter(RUNTIME_DIR),
    };
  })();
  return runtimePromise;
}

// Round counter (feeds the event log and gif frame ordering) plus wedge
// detection across rounds: RAM position and screen hash that never change
// mean the agent is walking into a wall or mashing a dead menu.
let round = 0;
let lastSignature = "";
let stuckRounds = 0;
let lastPosition = "";
let samePositionRounds = 0;

async function pokemonInstructions(context: TurnContext): Promise<Message[]> {
  const state = await runtime();
  round += 1;
  const frame = await emulator.frame();
  state.screenshots.save(round, frame.screen_hash, frame.screenshot_b64);

  const milestones = state.progress.observe(round, frame.state);
  for (const milestone of milestones) {
    state.events.append(round, "milestone", { milestone });
    // Auto-checkpoint every milestone so the agent can always rewind to the
    // last real progress point.
    void emulator.saveCheckpoint(`auto_r${round}`).catch(() => {});
  }

  const signature = `${positionSignature(frame)}:${frame.screen_hash}`;
  stuckRounds = signature === lastSignature ? stuckRounds + 1 : 0;
  lastSignature = signature;
  const position = positionSignature(frame);
  samePositionRounds = position === lastPosition ? samePositionRounds + 1 : 0;
  lastPosition = position;

  let directive: string | null = null;
  if (stuckRounds >= STUCK_ROUNDS_BEFORE_NUDGE) {
    directive = stuckDirective(stuckRounds);
  } else if (samePositionRounds >= SAME_POSITION_ROUNDS_BEFORE_NUDGE) {
    directive = samePositionDirective(samePositionRounds);
  }

  state.events.append(round, "round_started", {
    state: frame.state,
    directive: directive === null ? null : directive.split("\n", 1)[0],
  });

  return [
    ...context.agentConfig.instructions,
    ...(await buildInstructionMessages({
      systemPromptPath: SYSTEM_PROMPT_PATH,
      store: state.store,
      skillsIndex: await state.skills.index(),
      progress: state.progress,
      frame,
      directive,
    })),
  ];
}

async function registerPokemonTools(
  registry: HarnessToolRegistry,
  _context: TurnContext,
): Promise<void> {
  const state = await runtime();
  const toolContext = {
    emulator,
    log: (message: string) => console.log(`[tool] ${message}`),
  };
  const tools: AgentTool[] = [
    ...gameTools(emulator),
    ...selfTools(state.store),
    ...skillTools(state.skills),
    ...(await state.store.loadAgentTools(toolContext, (warning) =>
      console.warn(`[warn] ${warning}`),
    )),
  ];
  for (const tool of tools) {
    registry.register(adaptAgentTool(state, tool));
  }
}

// Bridges the evaluation's AgentTool shape onto exo's tool registry. Frames
// returned by tools are dropped — the instructions hook injects the live
// screen every round, so the model still sees every consequence.
function adaptAgentTool(state: Runtime, tool: AgentTool): ToolInstance {
  return {
    definition: {
      name: tool.name,
      description: tool.description,
      parameters: tool.parameters as JsonValue,
    },
    source: "library",
    handler: {
      async execute(args) {
        const result = await tool.execute(args);
        if (result.improvement !== undefined) {
          state.events.append(round, "improvement", {
            detail: result.improvement,
          });
          return `${result.text}\n[self-improvement] ${result.improvement}`;
        }
        return result.text;
      },
    },
  };
}

function positionSignature(frame: FramePayload): string {
  const state = frame.state;
  return `${state.map_id}:${state.x}:${state.y}:${state.in_battle}`;
}

export default defineHarness({
  async runTurn(context) {
    await runResponsesHarnessTurn(context, {
      instructions: pokemonInstructions,
      registerTools: registerPokemonTools,
    });
  },
});
