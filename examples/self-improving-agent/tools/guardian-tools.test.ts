import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { HarnessToolRegistry, type TurnContext } from "@exo/harness";
import { afterEach, describe, expect, it } from "vitest";

import {
  guardianStateDir,
  parseRebuildReason,
  rebuildAndRestartExoTool,
  registerGuardianTools,
  writeFailedOutcome,
} from "./guardian-tools";

const temporaryDirectories: string[] = [];

afterEach(() => {
  while (temporaryDirectories.length > 0) {
    rmSync(temporaryDirectories.pop() as string, {
      recursive: true,
      force: true,
    });
  }
});

describe("guardian tools", () => {
  it("uses the active Exo root for guardian state", () => {
    expect(guardianStateDir({ EXO_ROOT: "/tmp/eval-exo" }, "/src/exo")).toBe(
      "/tmp/eval-exo",
    );
    expect(guardianStateDir({}, "/src/exo")).toBe("/src/exo/.exo");
  });

  it("registers only rebuild_and_restart_exo", () => {
    const registry = new HarnessToolRegistry({} as TurnContext);

    registerGuardianTools(registry);

    expect(registry.definitions().map(({ name }) => name)).toEqual([
      "rebuild_and_restart_exo",
    ]);
  });

  it("defines the narrow asynchronous rebuild facade with a reason", () => {
    const tool = rebuildAndRestartExoTool();

    expect(tool.source).toBe("built_in");
    expect(tool.definition.name).toBe("rebuild_and_restart_exo");
    expect(tool.definition.parameters).toEqual({
      type: "object",
      additionalProperties: false,
      properties: {
        reason: {
          type: ["string", "null"],
          description:
            "Short free-text note describing why this rebuild was requested, for example the change being activated. Prefer a concrete description over null.",
        },
      },
      required: ["reason"],
    });
  });

  it("parses rebuild reasons for the durable update record", () => {
    expect(parseRebuildReason({ reason: "Add webhook adapter" })).toBe(
      "Add webhook adapter",
    );
    expect(parseRebuildReason({ reason: "  " })).toBeNull();
    expect(parseRebuildReason({ reason: null })).toBeNull();
    expect(() => parseRebuildReason({ reason: 12 })).toThrow(
      "rebuild_and_restart_exo reason must be a string or null",
    );
  });

  it("records a terminal failed outcome over a queued update", () => {
    const directory = mkdtempSync(join(tmpdir(), "guardian-outcome-"));
    temporaryDirectories.push(directory);
    const outcomePath = join(directory, "update.json");
    writeFileSync(
      outcomePath,
      `${JSON.stringify({ updateId: "u-1", status: "queued" })}\n`,
      "utf8",
    );

    writeFailedOutcome(outcomePath, {
      error: "failed to spawn deferred activation helper (ENOENT)",
    });

    const record = JSON.parse(readFileSync(outcomePath, "utf8")) as Record<
      string,
      unknown
    >;
    expect(record.updateId).toBe("u-1");
    expect(record.status).toBe("failed");
    expect(record.error).toBe(
      "failed to spawn deferred activation helper (ENOENT)",
    );
    expect(typeof record.failedAt).toBe("string");
  });

  it("records a terminal failed outcome even without a queued record", () => {
    const directory = mkdtempSync(join(tmpdir(), "guardian-outcome-"));
    temporaryDirectories.push(directory);
    const outcomePath = join(directory, "missing.json");

    writeFailedOutcome(outcomePath, { error: "helper missing" });

    const record = JSON.parse(readFileSync(outcomePath, "utf8")) as Record<
      string,
      unknown
    >;
    expect(record.status).toBe("failed");
    expect(record.error).toBe("helper missing");
  });
});
