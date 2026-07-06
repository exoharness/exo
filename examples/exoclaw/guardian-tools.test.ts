import { describe, expect, it } from "vitest";

import {
  clampOutput,
  guardianCommandArgs,
  parseGuardianArguments,
  shouldDeferAction,
} from "./guardian-tools";

describe("parseGuardianArguments", () => {
  it("accepts a supported action and applies defaults for null fields", () => {
    expect(
      parseGuardianArguments({
        action: "status",
        build: null,
        logTarget: null,
      }),
    ).toEqual({ action: "status", build: false, logTarget: "all" });
  });

  it("passes explicit build and logTarget through", () => {
    expect(
      parseGuardianArguments({
        action: "restart_all",
        build: true,
        logTarget: "scheduler",
      }),
    ).toEqual({ action: "restart_all", build: true, logTarget: "scheduler" });
  });

  it("rejects unsupported actions", () => {
    expect(() =>
      parseGuardianArguments({
        action: "reboot",
        build: null,
        logTarget: null,
      }),
    ).toThrow("guardian_action action must be a supported guardian action");
    expect(() =>
      parseGuardianArguments({ build: null, logTarget: null }),
    ).toThrow("guardian_action action must be a supported guardian action");
  });

  it("rejects non-boolean build and unknown logTarget values", () => {
    expect(() =>
      parseGuardianArguments({
        action: "status",
        build: "yes",
        logTarget: null,
      }),
    ).toThrow("guardian_action build must be boolean or null");
    expect(() =>
      parseGuardianArguments({
        action: "logs",
        build: null,
        logTarget: "kernel",
      }),
    ).toThrow(
      "guardian_action logTarget must be scheduler, adapters, all, or null",
    );
  });
});

describe("shouldDeferAction", () => {
  it("defers restart and stop actions so the current turn can finish", () => {
    expect(shouldDeferAction("restart_all")).toBe(true);
    expect(shouldDeferAction("restart_services")).toBe(true);
    expect(shouldDeferAction("restart_adapters")).toBe(true);
    expect(shouldDeferAction("restart_scheduler")).toBe(true);
    expect(shouldDeferAction("stop_services")).toBe(true);
  });

  it("runs read-only and additive actions immediately", () => {
    expect(shouldDeferAction("status")).toBe(false);
    expect(shouldDeferAction("build")).toBe(false);
    expect(shouldDeferAction("start_services")).toBe(false);
    expect(shouldDeferAction("logs")).toBe(false);
  });
});

describe("guardianCommandArgs", () => {
  it("adds --build only for restart_all with build requested", () => {
    expect(
      guardianCommandArgs({
        action: "restart_all",
        build: true,
        logTarget: "all",
      }),
    ).toEqual(["restart-all", "--build"]);
    expect(
      guardianCommandArgs({
        action: "restart_all",
        build: false,
        logTarget: "all",
      }),
    ).toEqual(["restart-all"]);
    // build is ignored for other actions.
    expect(
      guardianCommandArgs({ action: "build", build: true, logTarget: "all" }),
    ).toEqual(["build"]);
  });

  it("passes a specific log target and omits the default all", () => {
    expect(
      guardianCommandArgs({
        action: "logs",
        build: false,
        logTarget: "scheduler",
      }),
    ).toEqual(["logs", "scheduler"]);
    expect(
      guardianCommandArgs({
        action: "logs",
        build: false,
        logTarget: "adapters",
      }),
    ).toEqual(["logs", "adapters"]);
    expect(
      guardianCommandArgs({ action: "logs", build: false, logTarget: "all" }),
    ).toEqual(["logs"]);
  });

  it("maps snake_case actions to the guardian's kebab-case commands", () => {
    expect(
      guardianCommandArgs({
        action: "start_services",
        build: false,
        logTarget: "all",
      }),
    ).toEqual(["start-services"]);
  });
});

describe("clampOutput", () => {
  it("returns output at the limit unchanged", () => {
    const exact = "x".repeat(20_000);
    expect(clampOutput(exact)).toBe(exact);
    expect(clampOutput("")).toBe("");
  });

  it("truncates past the limit and reports the dropped size", () => {
    const clamped = clampOutput("x".repeat(20_005));
    expect(clamped).toBe(`${"x".repeat(20_000)}\n... truncated 5 chars`);
  });
});
