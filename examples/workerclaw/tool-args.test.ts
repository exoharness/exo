import { describe, expect, it } from "vitest";

import { unwrapHarnessToolArgs } from "./tool-args.js";

describe("unwrapHarnessToolArgs", () => {
  it("unwraps a single valid envelope", () => {
    expect(
      unwrapHarnessToolArgs({
        type: "valid",
        value: { type: "file", url: "https://example.com/a.pptx" },
      }),
    ).toEqual({ type: "file", url: "https://example.com/a.pptx" });
  });

  it("unwraps double-nested valid envelopes", () => {
    expect(
      unwrapHarnessToolArgs({
        type: "valid",
        value: {
          type: "valid",
          value: {
            type: "file",
            url: "https://drive.google.com/file/d/abc/view",
            label: "deck",
            content: null,
            filename: "deck.pptx",
            mimeType:
              "application/vnd.openxmlformats-officedocument.presentationml.presentation",
          },
        },
      }),
    ).toMatchObject({
      type: "file",
      url: "https://drive.google.com/file/d/abc/view",
    });
  });

  it("returns plain args unchanged", () => {
    const plain = { type: "url", url: "https://example.com" };
    expect(unwrapHarnessToolArgs(plain)).toEqual(plain);
  });
});
