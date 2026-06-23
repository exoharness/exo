// @vitest-environment jsdom
import "../test/setup.ts";
import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { MarkdownContent } from "./Markdown";
import { splitThinking } from "./Transcript";

describe("splitThinking", () => {
  it("extracts closed thinking blocks and leaves surrounding text", () => {
    const input = "Hello <think>step one</think> world";
    expect(splitThinking(input)).toEqual([
      { type: "text", text: "Hello" },
      { type: "think", text: "step one" },
      { type: "text", text: "world" },
    ]);
  });

  it("handles an unclosed thinking tag at the end", () => {
    const input = "visible <think>still reasoning";
    expect(splitThinking(input)).toEqual([
      { type: "text", text: "visible" },
      { type: "think", text: "still reasoning" },
    ]);
  });

  it("drops empty segments after trimming tag residue", () => {
    expect(splitThinking("<think>   </think>")).toEqual([]);
    expect(splitThinking("only text")).toEqual([
      { type: "text", text: "only text" },
    ]);
  });
});

describe("MarkdownContent", () => {
  it("renders fenced code inside a pre/code block", () => {
    const { container } = render(
      <MarkdownContent text={"```js\nconst n = 1;\n```"} />,
    );

    const pre = container.querySelector("pre");
    expect(pre).not.toBeNull();
    const code = pre?.querySelector("code");
    expect(code).not.toBeNull();
    expect(code?.textContent?.replace(/\s+/g, " ")).toContain("const n = 1");
    expect(screen.getByRole("button", { name: "copy" })).toBeInTheDocument();
  });

  it("renders GFM tables", () => {
    const tableMd = ["| left | right |", "| --- | --- |", "| a | b |"].join(
      "\n",
    );
    render(<MarkdownContent text={tableMd} />);

    const table = screen.getByRole("table");
    expect(
      within(table).getByRole("columnheader", { name: "left" }),
    ).toBeInTheDocument();
    expect(within(table).getByRole("cell", { name: "b" })).toBeInTheDocument();
  });

  it("renders inline math with KaTeX markup", () => {
    const { container } = render(
      <MarkdownContent text={"Energy is $E=mc^2$ here."} />,
    );

    const katex = container.querySelector(".katex");
    expect(katex).not.toBeNull();
    expect(katex?.textContent).toContain("E");
    expect(katex?.textContent).toContain("mc");
  });

  it("shows an empty placeholder for whitespace-only input", () => {
    render(<MarkdownContent text={"  \n  "} />);
    expect(screen.getByText("(empty)")).toBeInTheDocument();
  });
});
