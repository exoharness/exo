// Entry point: the self-improving RPG agent turn loop (Phantasy Star 1 on
// the Sega Master System via the EmulatorJS sidecar).
//
//   pnpm exec tsx agent/run.ts   (emulator sidecar must already be running;
//                                 use ../run.sh to start both)
//
// Env: OPENAI_API_KEY (required), RPG_MODEL (default gpt-5.4),
// RPG_REASONING (default low), RPG_TURNS (default 0 = run forever),
// RPG_EMULATOR_URL (default http://127.0.0.1:8777).

import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  bootstrapDirective,
  buildTurnInput,
  reflectionDirective,
  stuckDirective,
  type TurnRecord,
} from "./context";
import { EmulatorClient, type FramePayload } from "./emulator-client";
import { EventLog, ProgressTracker, ScreenshotWriter } from "./events";
import { gameTools } from "./game-tools";
import {
  callModel,
  functionCallOutput,
  imageMessage,
  textMessage,
  type ModelConfig,
} from "./model";
import { SelfStore, selfTools } from "./self-tools";
import { SkillsStore, skillTools } from "./skills";
import type { AgentTool } from "./tool-types";

const BASE_DIR = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "..",
);
const RUNTIME_DIR =
  process.env.RPG_RUNTIME_DIR !== undefined &&
  process.env.RPG_RUNTIME_DIR.length > 0
    ? path.resolve(process.env.RPG_RUNTIME_DIR)
    : path.join(BASE_DIR, "runtime");
const SYSTEM_PROMPT_PATH = path.join(BASE_DIR, "prompts", "system.md");
const SEED_PLAYBOOK_PATH = path.join(BASE_DIR, "prompts", "playbook.seed.md");
const HISTORY_PATH = path.join(RUNTIME_DIR, "history.json");

const MAX_ROUND_TRIPS_PER_TURN = 12;
const REFLECT_EVERY_TURNS = 10;
const STUCK_TURNS_BEFORE_NUDGE = 3;

async function main(): Promise<void> {
  const apiKey = process.env.OPENAI_API_KEY;
  if (apiKey === undefined || apiKey.length === 0) {
    console.error("OPENAI_API_KEY is not set");
    process.exit(1);
  }
  const modelConfig: ModelConfig = {
    apiKey,
    model: process.env.RPG_MODEL ?? "gpt-5.4",
    reasoningEffort: process.env.RPG_REASONING ?? "low",
    maxOutputTokens: 8_192,
  };
  const maxTurns = Number(process.env.RPG_TURNS ?? "0") || Infinity;
  const emulator = new EmulatorClient(
    process.env.RPG_EMULATOR_URL ?? "http://127.0.0.1:8777",
  );

  const health = await emulator.health();
  await fs.mkdir(RUNTIME_DIR, { recursive: true });
  const store = new SelfStore(RUNTIME_DIR);
  await store.init(await fs.readFile(SEED_PLAYBOOK_PATH, "utf8"));
  const skills = new SkillsStore(RUNTIME_DIR);
  await skills.init();
  const events = new EventLog(RUNTIME_DIR);
  const progress = new ProgressTracker(RUNTIME_DIR);
  const screenshots = new ScreenshotWriter(RUNTIME_DIR);
  const history = await loadHistory();

  let stopRequested = false;
  process.on("SIGINT", () => {
    if (stopRequested) {
      process.exit(130); // second ^C: hard exit
    }
    stopRequested = true;
    console.log("\n[stop requested — finishing current turn]");
  });

  console.log(
    `rpg agent: rom=${health.rom} core=${health.core} model=${modelConfig.model} ` +
      `turn=${history.length + 1} ${Number.isFinite(maxTurns) ? `(max ${maxTurns} this run)` : "(running until ^C)"}`,
  );

  let stuckTurns = 0;
  let lastScreenHash = "";
  let totalInputTokens = 0;
  let totalOutputTokens = 0;
  const startedAtTurn = history.length;

  while (!stopRequested && history.length - startedAtTurn < maxTurns) {
    const turn = history.length + 1;
    const openingFrame = await emulator.frame();
    screenshots.save(
      turn,
      openingFrame.screen_hash,
      openingFrame.screenshot_b64,
    );

    const turnMilestones = handleMilestones(
      progress,
      events,
      emulator,
      turn,
      openingFrame,
    );

    // Without RAM coordinates, an unchanged screen hash is the only stuck
    // signal. RPG screens usually animate (water, torches), so hash equality
    // across whole turns is a strong sign nothing is being accepted.
    stuckTurns =
      openingFrame.screen_hash === lastScreenHash ? stuckTurns + 1 : 0;

    let directive: string | null = null;
    if (turn === 1) {
      directive = bootstrapDirective();
    } else if (turn % REFLECT_EVERY_TURNS === 0) {
      directive = reflectionDirective();
    } else if (stuckTurns >= STUCK_TURNS_BEFORE_NUDGE) {
      directive = stuckDirective(stuckTurns);
    }

    events.append(turn, "turn_started", {
      screen_hash: openingFrame.screen_hash,
      directive: directive === null ? null : directive.split("\n", 1)[0],
    });

    // Rebuild the registry every turn so freshly installed agent tools appear.
    const toolContext = {
      emulator,
      log: (message: string) => console.log(`    [tool] ${message}`),
    };
    const tools: AgentTool[] = [
      ...gameTools(emulator),
      ...selfTools(store),
      ...skillTools(skills),
      ...(await store.loadAgentTools(toolContext, (warning) =>
        console.warn(`    [warn] ${warning}`),
      )),
    ];
    const toolsByName = new Map(tools.map((tool) => [tool.name, tool]));

    let input = await buildTurnInput({
      systemPromptPath: SYSTEM_PROMPT_PATH,
      store,
      skillsIndex: await skills.index(),
      progress,
      history,
      turn,
      frame: openingFrame,
      directive,
    });

    const improvements: string[] = [];
    let summary = "";
    let actionCount = 0;
    let latestFrame = openingFrame;

    for (
      let roundTrip = 0;
      roundTrip < MAX_ROUND_TRIPS_PER_TURN;
      roundTrip += 1
    ) {
      let response;
      try {
        response = await callModel(
          modelConfig,
          input,
          tools.map(({ name, description, parameters }) => ({
            name,
            description,
            parameters,
          })),
        );
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        events.append(turn, "error", { message });
        console.error(`    [error] ${message}`);
        summary = `(model error: ${message})`;
        break;
      }
      totalInputTokens += response.inputTokens;
      totalOutputTokens += response.outputTokens;

      if (response.toolCalls.length === 0) {
        summary = response.text || "(no summary)";
        break;
      }

      input = [...input, ...response.outputItems];
      for (const call of response.toolCalls) {
        actionCount += 1;
        const tool = toolsByName.get(call.name);
        let resultText: string;
        let resultFrame: FramePayload | undefined;
        if (tool === undefined) {
          resultText = `unknown tool '${call.name}'`;
        } else {
          try {
            const result = await tool.execute(call.arguments);
            resultText = result.text;
            resultFrame = result.frame;
            if (result.improvement !== undefined) {
              improvements.push(result.improvement);
              events.append(turn, "improvement", {
                detail: result.improvement,
              });
            }
            if (tool.attachFrameAfter === true && resultFrame === undefined) {
              resultFrame = await emulator.frame();
            }
          } catch (error) {
            resultText = `tool failed: ${error instanceof Error ? error.message : String(error)}`;
          }
        }
        events.append(turn, "tool_call", {
          name: call.name,
          arguments: call.arguments,
          result: resultText.slice(0, 2_000),
        });
        input.push(functionCallOutput(call.callId, resultText));
        if (resultFrame !== undefined) {
          latestFrame = resultFrame;
          screenshots.save(
            turn,
            resultFrame.screen_hash,
            resultFrame.screenshot_b64,
          );
          turnMilestones.push(
            ...handleMilestones(progress, events, emulator, turn, resultFrame),
          );
          input.push(
            imageMessage(
              `Screen after ${call.name}:`,
              resultFrame.screenshot_b64,
            ),
          );
        }
      }
      if (roundTrip === MAX_ROUND_TRIPS_PER_TURN - 1) {
        summary =
          `(round-trip cap hit after ${actionCount} actions) ${response.text}`.trim();
      } else if (roundTrip === MAX_ROUND_TRIPS_PER_TURN - 2) {
        input.push(
          textMessage(
            "developer",
            "Round-trip budget for this turn is nearly exhausted. Stop calling tools and reply with your turn summary: what you did, what you learned, what to do next turn.",
          ),
        );
      }
    }

    lastScreenHash = latestFrame.screen_hash;
    const record: TurnRecord = {
      turn,
      summary: summary.slice(0, 600),
      milestones: turnMilestones,
      improvements,
    };
    history.push(record);
    await saveHistory(history);
    events.append(turn, "turn_summary", record);

    narrate(record, progress, actionCount, totalInputTokens, totalOutputTokens);
  }

  console.log(
    `stopped at turn ${history.length}; tokens in=${totalInputTokens} out=${totalOutputTokens}`,
  );
  console.log(`progress: ${progress.summary()}`);
}

function handleMilestones(
  progress: ProgressTracker,
  events: EventLog,
  emulator: EmulatorClient,
  turn: number,
  frame: FramePayload,
): string[] {
  const milestones = progress.observe(turn, frame.screen_hash);
  for (const milestone of milestones) {
    console.log(`  ★ MILESTONE: ${milestone}`);
    events.append(turn, "milestone", { milestone });
  }
  if (milestones.length > 0) {
    // Auto-checkpoint every milestone so the agent can always rewind to the
    // last real progress point.
    void emulator.saveCheckpoint(`auto_t${turn}`).catch(() => {});
  }
  return milestones;
}

function narrate(
  record: TurnRecord,
  progress: ProgressTracker,
  actionCount: number,
  inputTokens: number,
  outputTokens: number,
): void {
  const markers = record.improvements.length
    ? ` [${record.improvements.join("] [")}]`
    : "";
  console.log(
    `T${String(record.turn).padStart(3, "0")} screens=${progress.distinctScreens()} ` +
      `actions=${actionCount} tok=${Math.round(inputTokens / 1000)}k/${Math.round(outputTokens / 1000)}k${markers}`,
  );
  console.log(`     ${record.summary.split("\n")[0].slice(0, 160)}`);
}

async function loadHistory(): Promise<TurnRecord[]> {
  try {
    const parsed = JSON.parse(await fs.readFile(HISTORY_PATH, "utf8"));
    return Array.isArray(parsed) ? parsed : [];
  } catch {
    return [];
  }
}

async function saveHistory(history: TurnRecord[]): Promise<void> {
  await fs.writeFile(
    HISTORY_PATH,
    `${JSON.stringify(history, null, 2)}\n`,
    "utf8",
  );
}

await main();
