// @vitest-environment jsdom
import "../test/setup.ts";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import type { Artifact } from "../api/protocol";
import {
  ArtifactContext,
  ArtifactView,
  classifyArtifact,
  findArtifactRef,
} from "./ArtifactViewer";

function artifact(
  path: string,
  contents: number[],
  overrides: Partial<Artifact> = {},
): Artifact {
  return {
    artifact_id: "art-1",
    path,
    version: 1,
    created_at: "2025-06-01T12:00:00.000Z",
    size_bytes: contents.length,
    contents,
    ...overrides,
  };
}

function utf8(text: string): number[] {
  return [...new TextEncoder().encode(text)];
}

describe("classifyArtifact", () => {
  it("classifies by image extension", () => {
    expect(classifyArtifact(artifact("photo.png", utf8("not an image")))).toBe(
      "image",
    );
    expect(classifyArtifact(artifact("pic.JPG", []))).toBe("image");
    expect(classifyArtifact(artifact("icon.svg", utf8("<svg/>")))).toBe(
      "image",
    );
  });

  it("sniffs PNG, JPEG, and GIF magic bytes without a known extension", () => {
    const png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    const jpeg = [0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10];
    const gif = [0x47, 0x49, 0x46, 0x38, 0x39, 0x61];

    expect(classifyArtifact(artifact("blob", png))).toBe("image");
    expect(classifyArtifact(artifact("upload.bin", jpeg))).toBe("image");
    expect(classifyArtifact(artifact("data", gif))).toBe("image");
  });

  it("classifies json and markdown by extension", () => {
    expect(classifyArtifact(artifact("config.json", utf8('{"a":1}')))).toBe(
      "json",
    );
    expect(classifyArtifact(artifact("notes.md", utf8("# hi")))).toBe(
      "markdown",
    );
    expect(classifyArtifact(artifact("README.markdown", utf8("text")))).toBe(
      "markdown",
    );
  });

  it("classifies video and audio by extension", () => {
    expect(classifyArtifact(artifact("clip.mp4", []))).toBe("video");
    expect(classifyArtifact(artifact("render.webm", []))).toBe("video");
    expect(classifyArtifact(artifact("voice.mp3", []))).toBe("audio");
    expect(classifyArtifact(artifact("speech.wav", []))).toBe("audio");
    expect(classifyArtifact(artifact("song.flac", []))).toBe("audio");
  });

  it("sniffs extensionless JSON by leading brace or bracket", () => {
    expect(classifyArtifact(artifact("noext", utf8('  {"k":1}')))).toBe("json");
    expect(classifyArtifact(artifact("", utf8("[1,2]")))).toBe("json");
  });

  it("falls back to text for plain files", () => {
    expect(classifyArtifact(artifact("log.txt", utf8("hello")))).toBe("text");
    expect(classifyArtifact(artifact("data", utf8("plain prose")))).toBe(
      "text",
    );
  });

  it("strips query/hash from path before reading extension", () => {
    expect(classifyArtifact(artifact("shot.png?v=2#frag", utf8("x")))).toBe(
      "image",
    );
    expect(classifyArtifact(artifact("cfg.json?dl=1", utf8("{}")))).toBe(
      "json",
    );
  });
});

describe("ArtifactView", () => {
  it("renders a view affordance with path when no custom label", () => {
    render(
      <ArtifactView artifactId="art-42" path="outputs/chart.png" version={3} />,
    );

    expect(
      screen.getByRole("button", { name: /view artifact/i }),
    ).toBeInTheDocument();
    expect(screen.getByText("outputs/chart.png")).toBeInTheDocument();
  });

  it("uses a custom trigger label and toggles open state", async () => {
    const user = userEvent.setup();
    const load = () =>
      Promise.resolve({
        artifact_id: "art-42",
        path: "x.txt",
        version: 1,
        created_at: "2025-06-01T12:00:00.000Z",
        size_bytes: 2,
        contents: [104, 105],
      });

    render(
      <ArtifactContext.Provider value={{ load }}>
        <ArtifactView
          artifactId="art-42"
          path="x.txt"
          triggerLabel="inspect"
          version={1}
        />
      </ArtifactContext.Provider>,
    );

    const button = screen.getByRole("button", { name: "inspect" });
    await user.click(button);
    expect(screen.getByRole("button", { name: "hide" })).toBeInTheDocument();
    expect(await screen.findByText("hi")).toBeInTheDocument();
  });
});

describe("findArtifactRef", () => {
  it("finds nested artifact_id pointers", () => {
    expect(
      findArtifactRef({
        meta: { items: [{ artifact_id: "a-1", version: 2, path: "p" }] },
      }),
    ).toEqual({
      artifactId: "a-1",
      version: 2,
      path: "p",
    });
  });

  it("returns null when no artifact reference exists", () => {
    expect(findArtifactRef({ ok: true })).toBeNull();
    expect(findArtifactRef(null)).toBeNull();
  });
});
