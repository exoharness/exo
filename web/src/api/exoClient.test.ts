import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  EVENT_PAGE_SIZE,
  normalizeHealthEndpoint,
  normalizeRequestEndpoint,
} from "./exoClient";

describe("normalizeRequestEndpoint", () => {
  const origin = "http://localhost:5173";

  beforeEach(() => {
    vi.stubGlobal("window", { location: { origin } });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("appends /request to a bare host", () => {
    expect(normalizeRequestEndpoint("http://127.0.0.1:4766")).toBe(
      "http://127.0.0.1:4766/request",
    );
  });

  it("preserves an existing /request suffix including trailing slash", () => {
    expect(normalizeRequestEndpoint("http://127.0.0.1:4766/request/")).toBe(
      "http://127.0.0.1:4766/request/",
    );
  });

  it("strips query and hash before normalizing", () => {
    expect(
      normalizeRequestEndpoint("http://127.0.0.1:4766/exo?token=abc#frag"),
    ).toBe("http://127.0.0.1:4766/exo/request");
  });

  it("adds http scheme to bare hostnames", () => {
    expect(normalizeRequestEndpoint("api.example.com")).toBe(
      "http://api.example.com/request",
    );
  });

  it("resolves root-relative paths against window.location.origin", () => {
    expect(normalizeRequestEndpoint("/exo")).toBe(`${origin}/exo/request`);
  });

  it("throws for empty base URLs", () => {
    expect(() => normalizeRequestEndpoint("   ")).toThrow("base URL is empty");
  });
});

describe("normalizeHealthEndpoint", () => {
  beforeEach(() => {
    vi.stubGlobal("window", { location: { origin: "http://localhost:5173" } });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("maps /request bases to sibling /health", () => {
    expect(normalizeHealthEndpoint("http://127.0.0.1:4766/request")).toBe(
      "http://127.0.0.1:4766/health",
    );
  });

  it("appends /health to arbitrary paths", () => {
    expect(normalizeHealthEndpoint("http://127.0.0.1:4766/exo/")).toBe(
      "http://127.0.0.1:4766/exo/health",
    );
  });
});

describe("EVENT_PAGE_SIZE", () => {
  it("matches the client page size used by event polling", () => {
    expect(EVENT_PAGE_SIZE).toBe(500);
  });
});
