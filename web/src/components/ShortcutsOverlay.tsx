import { useEffect, useMemo } from "react";
import { createPortal } from "react-dom";

interface ShortcutsOverlayProps {
  open: boolean;
  onClose: () => void;
}

interface ShortcutEntry {
  keys: string[];
  description: string;
}

interface ShortcutGroup {
  title: string;
  items: ShortcutEntry[];
}

function isMacPlatform(): boolean {
  return (
    typeof navigator !== "undefined" &&
    /Mac|iPhone|iPod|iPad/i.test(navigator.platform)
  );
}

function ShortcutKeys({ keys }: { keys: string[] }) {
  return (
    <span className="shortcut-keys">
      {keys.map((key, index) => (
        <kbd className="command-palette-kbd" key={`${key}-${index}`}>
          {key}
        </kbd>
      ))}
    </span>
  );
}

export function ShortcutsOverlay({ open, onClose }: ShortcutsOverlayProps) {
  const mod = isMacPlatform() ? "⌘" : "Ctrl";

  const groups = useMemo<ShortcutGroup[]>(
    () => [
      {
        title: "Global",
        items: [
          { keys: [mod, "K"], description: "Toggle command palette" },
          { keys: ["?"], description: "Show keyboard shortcuts" },
          {
            keys: ["Enter"],
            description: "Commit base URL (when base field is focused)",
          },
        ],
      },
      {
        title: "Chat composer",
        items: [
          { keys: ["Enter"], description: "Send message" },
          { keys: ["⇧", "Enter"], description: "New line" },
        ],
      },
      {
        title: "Find in conversation",
        items: [
          {
            keys: ["Enter"],
            description: "Next match (when find field is focused)",
          },
          {
            keys: ["⇧", "Enter"],
            description: "Previous match (when find field is focused)",
          },
          {
            keys: ["Esc"],
            description: "Clear find (when find field is focused)",
          },
        ],
      },
      {
        title: "Command palette",
        items: [
          { keys: ["↑"], description: "Move selection up" },
          { keys: ["↓"], description: "Move selection down" },
          { keys: ["Enter"], description: "Run selected command" },
          { keys: ["Esc"], description: "Close palette" },
        ],
      },
      {
        title: "This overlay",
        items: [{ keys: ["Esc"], description: "Close" }],
      },
    ],
    [mod],
  );

  useEffect(() => {
    if (!open) {
      return;
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [open, onClose]);

  if (!open) {
    return null;
  }

  return createPortal(
    <div
      className="command-palette-backdrop shortcuts-overlay-backdrop"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div
        className="command-palette shortcuts-overlay"
        role="dialog"
        aria-modal="true"
        aria-label="Keyboard shortcuts"
      >
        <div className="command-palette-input-row shortcuts-overlay-header">
          <h2 className="shortcuts-overlay-title">Keyboard shortcuts</h2>
          <kbd className="command-palette-kbd">esc</kbd>
        </div>
        <div className="command-palette-list shortcuts-overlay-list">
          {groups.map((group) => (
            <div className="command-group" key={group.title}>
              <div className="command-group-header">{group.title}</div>
              {group.items.map((item) => (
                <div className="shortcut-item" key={item.description}>
                  <span className="shortcut-item-label">
                    {item.description}
                  </span>
                  <ShortcutKeys keys={item.keys} />
                </div>
              ))}
            </div>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}
