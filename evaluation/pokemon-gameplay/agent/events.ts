// Append-only event log (canonical history — no agent tool can touch it,
// same rule as exo proper), plus RAM-derived milestone tracking and per-frame
// screenshot capture for the demo gif.

import fs from "node:fs";
import path from "node:path";

import type { GameState } from "./emulator-client";

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
  visitedMaps: number[];
  badgeCount: number;
  partyCount: number;
  maxPartyLevel: number;
  pokedexOwned: number;
}

export class ProgressTracker {
  private readonly progressPath: string;
  private readonly statePath: string;
  private state: ProgressState;

  constructor(runtimeDir: string) {
    this.progressPath = path.join(runtimeDir, "progress.jsonl");
    this.statePath = path.join(runtimeDir, "progress-state.json");
    this.state = this.load();
  }

  private load(): ProgressState {
    try {
      return JSON.parse(
        fs.readFileSync(this.statePath, "utf8"),
      ) as ProgressState;
    } catch {
      return {
        visitedMaps: [],
        badgeCount: 0,
        partyCount: 0,
        maxPartyLevel: 0,
        pokedexOwned: 0,
      };
    }
  }

  // Compares the new RAM state against high-water marks and returns any new
  // milestones. Purely objective: the model cannot claim progress it has not
  // made.
  observe(turn: number, game: GameState): string[] {
    const milestones: string[] = [];
    if (!this.state.visitedMaps.includes(game.map_id)) {
      this.state.visitedMaps.push(game.map_id);
      milestones.push(`Entered ${game.map_name} for the first time`);
    }
    if (game.badge_count > this.state.badgeCount) {
      const newest = game.badges[game.badges.length - 1] ?? "?";
      milestones.push(`Earned the ${newest} badge (${game.badge_count} total)`);
      this.state.badgeCount = game.badge_count;
    }
    if (game.party.length > this.state.partyCount) {
      const newest = game.party[game.party.length - 1];
      milestones.push(
        `Party grew to ${game.party.length}: ${newest?.species ?? "?"} L${newest?.level ?? "?"}`,
      );
      this.state.partyCount = game.party.length;
    }
    const maxLevel = Math.max(0, ...game.party.map((mon) => mon.level));
    if (this.state.maxPartyLevel > 0 && maxLevel > this.state.maxPartyLevel) {
      milestones.push(`Highest party level is now ${maxLevel}`);
    }
    if (maxLevel > this.state.maxPartyLevel) {
      this.state.maxPartyLevel = maxLevel;
    }
    if (game.pokedex_owned > this.state.pokedexOwned) {
      milestones.push(`Pokedex grew to ${game.pokedex_owned} owned`);
      this.state.pokedexOwned = game.pokedex_owned;
    }
    if (milestones.length > 0) {
      this.persist();
      for (const milestone of milestones) {
        fs.appendFileSync(
          this.progressPath,
          `${JSON.stringify({ ts: new Date().toISOString(), turn, milestone })}\n`,
          "utf8",
        );
      }
    }
    return milestones;
  }

  summary(): string {
    return (
      `maps visited: ${this.state.visitedMaps.length}, badges: ${this.state.badgeCount}, ` +
      `party size: ${this.state.partyCount}, top level: ${this.state.maxPartyLevel}, ` +
      `pokedex owned: ${this.state.pokedexOwned}`
    );
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
