#!/usr/bin/env python3
"""Update the checked-in default image pins from a sandbox image release."""

import re
import sys
from pathlib import Path


IMAGE_REFERENCES = {
    "codex-devbox": {
        "crates/executor/src/managed_agents/config.rs": 1,
        "crates/executor/src/managed_agents/tests.rs": 1,
    },
    "claude-code-devbox": {
        "crates/executor/src/managed_agents/config.rs": 1,
    },
    "pi-devbox": {
        "crates/executor/src/managed_agents/config.rs": 1,
    },
}


def main(digests_dir: Path) -> None:
    updates: dict[Path, str] = {}
    for image, references in IMAGE_REFERENCES.items():
        digest = (digests_dir / f"{image}.txt").read_text().strip()
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise ValueError(f"invalid digest for {image}: {digest!r}")
        image_ref = f"ghcr.io/exoharness/{image}"
        pattern = re.compile(rf"{re.escape(image_ref)}@sha256:[0-9a-f]{{64}}")
        for filename, expected_count in references.items():
            path = Path(filename)
            source = updates.get(path, path.read_text())
            updated, count = pattern.subn(f"{image_ref}@{digest}", source)
            if count != expected_count:
                raise ValueError(f"expected {expected_count} {image} reference(s) in {path}, found {count}")
            updates[path] = updated

    for path, updated in updates.items():
        path.write_text(updated)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: update-default-image-digests.py DIGESTS_DIR")
    main(Path(sys.argv[1]))
