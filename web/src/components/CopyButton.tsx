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
  const [copied, setCopied] = useState(false);
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
    if (!ok) {
      return;
    }
    setCopied(true);
    if (resetRef.current != null) {
      window.clearTimeout(resetRef.current);
    }
    resetRef.current = window.setTimeout(() => setCopied(false), 1400);
  }

  return (
    <button
      aria-label={copied ? "Copied" : label}
      className={className}
      onClick={(event) => void handleCopy(event)}
      type="button"
    >
      {copied ? "copied" : label}
    </button>
  );
}
