export async function copyText(text: string): Promise<boolean> {
  if (!text) {
    return false;
  }
  // Preferred path: the async Clipboard API. It rejects without a secure context,
  // a user gesture, or document focus, so on failure fall back to the legacy
  // hidden-textarea + execCommand approach rather than silently doing nothing.
  if (navigator.clipboard?.writeText) {
    try {
      await navigator.clipboard.writeText(text);
      return true;
    } catch {
      // fall through to the legacy path
    }
  }
  return legacyCopy(text);
}

function legacyCopy(text: string): boolean {
  if (typeof document === "undefined") {
    return false;
  }
  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "-9999px";
  textarea.style.opacity = "0";
  document.body.appendChild(textarea);

  // Preserve whatever the user had selected so copying does not clobber it.
  const selection = document.getSelection();
  const previousRange =
    selection && selection.rangeCount > 0 ? selection.getRangeAt(0) : null;

  textarea.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }

  document.body.removeChild(textarea);
  if (previousRange && selection) {
    selection.removeAllRanges();
    selection.addRange(previousRange);
  }
  return ok;
}
