// @vitest-environment jsdom
import "../test/setup.ts";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { copyText } from "../lib/copy";
import { CopyButton } from "./CopyButton";

vi.mock("../lib/copy", () => ({
  copyText: vi.fn(),
}));

describe("CopyButton", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.mocked(copyText).mockResolvedValue(true);
  });

  afterEach(() => {
    cleanup();
    vi.clearAllMocks();
    vi.useRealTimers();
  });

  it("copies text and shows a transient copied state", async () => {
    render(<CopyButton text="payload" />);
    const button = screen.getByRole("button", { name: "copy" });

    fireEvent.click(button);
    await act(async () => {
      await Promise.resolve();
    });

    expect(copyText).toHaveBeenCalledWith("payload");
    expect(button).toHaveAccessibleName("Copied");
    expect(button).toHaveTextContent("copied");

    await act(async () => {
      vi.advanceTimersByTime(1400);
    });

    expect(button).toHaveAccessibleName("copy");
    expect(button).toHaveTextContent("copy");
  });

  it("does not enter copied state when clipboard write fails", async () => {
    vi.mocked(copyText).mockResolvedValueOnce(false);
    render(<CopyButton text="secret" />);
    const button = screen.getByRole("button", { name: "copy" });

    fireEvent.click(button);
    await act(async () => {
      await Promise.resolve();
    });

    expect(copyText).toHaveBeenCalledWith("secret");
    expect(button).toHaveAccessibleName("copy");
    expect(button).toHaveTextContent("copy");
  });
});
