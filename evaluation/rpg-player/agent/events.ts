// Append-only event log (canonical history — no agent tool can touch it,
// same rule as exo proper), plus objective progress tracking and per-frame
// screenshot capture for the demo gif.
//
// Without RAM access (EmulatorJS does not expose structured memory reads
// across cores) the objective progress signal is screen novelty: the count
// of distinct screens ever seen. It cannot be inflated by standing still or
// looping the same rooms, and the model cannot claim it — the hash comes
// from the harness. Model-claimed milestones (claim_milestone) live in the
// event log, clearly labeled as claims.

import fs from "node:fs";
import path from "node:path";

export class EventLog {
  private readonly eventsPath: string;

  constructor(runtimeDir: string) {
    this.eventsPath = path.join(runtimeDir, "events.jsonl");
  }

  append(turn: number, type: string, data: unknown): void {
    fs.appendFileSync(
      this.eventsPath,
      `${JSON.stringify({ ts: new Date().toISOString(), turn, type, data })}\n`,
      "utf8",
    );
  }
}

export class ScreenshotWriter {
  private readonly dir: string;
  private sequence = 0;

  constructor(runtimeDir: string) {
    this.dir = path.join(runtimeDir, "screenshots");
    fs.mkdirSync(this.dir, { recursive: true });
    // Continue numbering across restarts so gif frames stay ordered.
    for (const file of fs.readdirSync(this.dir)) {
      const match = /^frame-(\d{6})/.exec(file);
      if (match !== null) {
        this.sequence = Math.max(this.sequence, Number(match[1]) + 1);
      }
    }
  }

  // Deduplicates identical consecutive screens so idle round trips do not
  // bloat the gif.
  private lastHash = "";

  save(turn: number, screenHash: string, pngBase64: string): void {
    if (screenHash === this.lastHash) {
      return;
    }
    this.lastHash = screenHash;
    const name = `frame-${String(this.sequence).padStart(6, "0")}-t${turn}.png`;
    fs.writeFileSync(
      path.join(this.dir, name),
      Buffer.from(pngBase64, "base64"),
    );
    this.sequence += 1;
  }
}

interface ProgressState {
  seenScreenHashes: string[];
  lastThreshold: number;
}

// Milestone every time the distinct-screen count crosses one of these.
const NOVELTY_THRESHOLDS = [
  10, 25, 50, 100, 200, 350, 500, 750, 1_000, 1_500, 2_000, 3_000, 5_000,
];

export class ProgressTracker {
  private readonly progressPath: string;
  private readonly statePath: string;
  private state: ProgressState;
  private readonly seen: Set<string>;

  constructor(runtimeDir: string) {
    this.progressPath = path.join(runtimeDir, "progress.jsonl");
    this.statePath = path.join(runtimeDir, "progress-state.json");
    this.state = this.load();
    this.seen = new Set(this.state.seenScreenHashes);
  }

  private load(): ProgressState {
    try {
      return JSON.parse(
        fs.readFileSync(this.statePath, "utf8"),
      ) as ProgressState;
    } catch {
      return { seenScreenHashes: [], lastThreshold: 0 };
    }
  }

  // Counts screens the agent has never seen before and emits a milestone
  // whenever the total crosses a threshold. Purely objective: the hash is
  // computed by the sidecar from the actual frame.
  observe(turn: number, screenHash: string): string[] {
    if (screenHash.length === 0 || this.seen.has(screenHash)) {
      return [];
    }
    this.seen.add(screenHash);
    this.state.seenScreenHashes.push(screenHash);
    const milestones: string[] = [];
    for (const threshold of NOVELTY_THRESHOLDS) {
      if (threshold > this.state.lastThreshold && this.seen.size >= threshold) {
        milestones.push(`Explored ${threshold} distinct screens`);
        this.state.lastThreshold = threshold;
      }
    }
    this.persist();
    for (const milestone of milestones) {
      fs.appendFileSync(
        this.progressPath,
        `${JSON.stringify({ ts: new Date().toISOString(), turn, milestone })}\n`,
        "utf8",
      );
    }
    return milestones;
  }

  distinctScreens(): number {
    return this.seen.size;
  }

  summary(): string {
    return `distinct screens seen: ${this.seen.size}`;
  }

  recentMilestones(limit: number): string[] {
    try {
      const lines = fs
        .readFileSync(this.progressPath, "utf8")
        .trim()
        .split("\n");
      return lines.slice(-limit).map((line) => {
        const parsed = JSON.parse(line) as { turn: number; milestone: string };
        return `turn ${parsed.turn}: ${parsed.milestone}`;
      });
    } catch {
      return [];
    }
  }

  private persist(): void {
    fs.writeFileSync(
      this.statePath,
      `${JSON.stringify(this.state, null, 2)}\n`,
      "utf8",
    );
  }
}
