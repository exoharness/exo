import { useEffect, useRef, useState, type MouseEvent } from "react";
import { copyText } from "../lib/copy";

export function CopyButton({
  text,
  label = "copy",
  className = "copy-button",
}: {
  text: string;
  label?: string;
  className?: string;
}) {
  const [status, setStatus] = useState<"idle" | "copied" | "failed">("idle");
  const resetRef = useRef<number | null>(null);

  useEffect(() => {
    return () => {
      if (resetRef.current != null) {
        window.clearTimeout(resetRef.current);
      }
    };
  }, []);

  async function handleCopy(event: MouseEvent<HTMLButtonElement>) {
    event.preventDefault();
    event.stopPropagation();
    const ok = await copyText(text);
    // Always surface the outcome: a silent no-op on failure is exactly what makes a
    // copy button feel broken.
    setStatus(ok ? "copied" : "failed");
    if (resetRef.current != null) {
      window.clearTimeout(resetRef.current);
    }
    resetRef.current = window.setTimeout(
      () => setStatus("idle"),
      ok ? 1400 : 2200,
    );
  }

  const text_ =
    status === "copied"
      ? "copied"
      : status === "failed"
        ? "copy failed"
        : label;
  return (
    <button
      aria-label={
        status === "copied"
          ? "Copied"
          : status === "failed"
            ? "Copy failed"
            : label
      }
      className={className}
      data-copy-status={status}
      onClick={(event) => void handleCopy(event)}
      type="button"
    >
      {text_}
    </button>
  );
}
