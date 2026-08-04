import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

const setupPrompt = readFileSync(
  join(dirname(fileURLToPath(import.meta.url)), "setup-prompt.md"),
  "utf8",
);

describe("exochat setup-prompt messaging (#167)", () => {
  it("asks the agent to reprint the ExoChat URL in the reply", () => {
    expect(setupPrompt).toMatch(/print the ExoChat URL again/i);
  });

  it("tells the user they can use the terminal UI or ExoChat", () => {
    expect(setupPrompt).toMatch(/terminal UI/i);
    expect(setupPrompt).toMatch(/ExoChat URL/i);
    expect(setupPrompt).toMatch(/either/i);
  });

  it("explains ExoChat survives closing the terminal chat", () => {
    expect(setupPrompt).toMatch(/keeps working even if/i);
    expect(setupPrompt).toMatch(/\/exit/i);
  });

  it("does not stop at only mentioning a text-only control channel", () => {
    // Regression guard for the old one-liner that skipped dual-channel UX.
    expect(setupPrompt).not.toMatch(
      /Briefly tell me the adapter id and that ExoChat is currently a text-only control channel/i,
    );
  });
});
