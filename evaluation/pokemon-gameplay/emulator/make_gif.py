"""Stitch the run's screenshots into an animated GIF.

Usage (from evaluation/pokemon-gameplay, venv active or via run.sh's venv):
  python emulator/make_gif.py [--out runtime/run.gif] [--fps 6] [--max-frames 600]

Frames come from runtime/screenshots/frame-*.png (written in order by the
agent, deduplicated on identical screens). When there are more frames than
--max-frames, frames are sampled evenly so the GIF still covers the whole
run.
"""

from __future__ import annotations

import argparse
from pathlib import Path

from PIL import Image

BASE_DIR = Path(__file__).resolve().parent.parent


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out", type=Path, default=BASE_DIR / "runtime" / "run.gif")
    parser.add_argument("--fps", type=float, default=6.0)
    parser.add_argument("--max-frames", type=int, default=600)
    parser.add_argument(
        "--screenshots",
        type=Path,
        default=BASE_DIR / "runtime" / "screenshots",
    )
    args = parser.parse_args()

    paths = sorted(args.screenshots.glob("frame-*.png"))
    if len(paths) == 0:
        raise SystemExit(f"no frames found in {args.screenshots}")

    if len(paths) > args.max_frames:
        step = len(paths) / args.max_frames
        paths = [paths[int(i * step)] for i in range(args.max_frames)]

    # Halve the 3x-upscaled frames so the GIF stays a reasonable size.
    frames = []
    for path in paths:
        image = Image.open(path).convert("RGB")
        frames.append(image.resize((image.width // 2, image.height // 2), resample=0))

    duration_ms = int(1000 / args.fps)
    frames[0].save(
        args.out,
        save_all=True,
        append_images=frames[1:],
        duration=duration_ms,
        loop=0,
        optimize=True,
    )
    print(f"wrote {args.out} ({len(frames)} frames, {args.out.stat().st_size // 1024} KiB)")


if __name__ == "__main__":
    main()
