# Pokémon — exo plays a Game Boy game from screenshots

Wires up **exo (gpt-5.5, vision) playing Pokémon** on a Game Boy emulator: each
turn exo _sees a screenshot_ and _chooses button presses_, the emulator advances,
and the loop repeats. It's the classic "agent plays Pokémon" demo — a fun,
legible test of vision + sequential decision-making + memory.

## How it works

Unlike the other evals there's no external benchmark — just an emulator and the
agent loop. [PyBoy](https://github.com/Baekalfen/PyBoy) is the scriptable Game Boy
emulator; `pokemon_runner.py` is the driver:

1. Capture the emulator screen → write it to `/tmp/exo-pokemon/screen.png`.
2. Run one exo turn. The **`harness-pokemon.ts`** harness reads that screenshot,
   injects it as an **image** user-message, and asks for the next button(s).
   gpt-5.5 (vision) replies with JSON `{"buttons": ["a","up",...]}`.
3. The driver parses the buttons and presses them in PyBoy
   (`pyboy.button(...)` + frame advance).
4. Repeat. One **persistent exo conversation** runs the whole session, so exo
   accumulates context across turns. The current screenshot is injected fresh each
   turn (not stored in history), so image cost stays flat while the text history
   (past choices) carries memory forward.

No tools / no exo sandbox involvement: the emulator lives in the Python driver,
exo is purely the brain. (Vision verified end-to-end against gpt-5.5 before this
was built.)

## Bring your own ROM

ROMs are copyrighted and are **not** included. Supply one you legally own:

```bash
export POKEMON_ROM=/path/to/pokemon.gb     # Pokémon Red/Blue/Yellow .gb (or .gbc)
```

## Quickstart

```bash
./setup.sh                                                   # uv venv + PyBoy + exo binary
OPENAI_API_KEY=… POKEMON_ROM=/path/to/rom.gb ./run.sh        # play ~40 turns
OPENAI_API_KEY=… POKEMON_ROM=… POKEMON_STEPS=150 ./run.sh    # longer
OPENAI_API_KEY=… POKEMON_ROM=… POKEMON_STATE=save.state ./run.sh   # start mid-game
```

Knobs (env / args): `POKEMON_STEPS`, `MODEL` (default `gpt-5.5`), `POKEMON_STATE`
(PyBoy save state to skip the intro), `--press-frames`, `--settle-frames`,
`--boot-frames`. Output (frames + `session.json` with every turn's buttons +
reasoning) lands in `runs/latest/` (gitignored).

## Watch it live (in your browser)

`live_server.py` is a tiny stdlib web view — the game screen + the agent's current
buttons, reasoning, and durable memory, auto-refreshing each turn:

```bash
# on the box: start the viewer, then start a game in another shell
python live_server.py --port 8080
OPENAI_API_KEY=… POKEMON_ROM=… ./run.sh --steps 300

# on your laptop: forward the port and open it
ssh -L 8080:localhost:8080 <box>     # then visit http://localhost:8080
```

The runner writes `/tmp/exo-pokemon/screen.png` + `state.json` each turn; the
server just reflects them, so it works for any in-progress run.

## Show it working

Every turn's frame is saved to `runs/latest/frames/NNNN.png`. Make a GIF/video:

```bash
# GIF (ImageMagick) or MP4 (ffmpeg) — whichever you have
convert -delay 25 runs/latest/frames/*.png runs/latest/play.gif
ffmpeg -framerate 8 -i runs/latest/frames/%04d.png runs/latest/play.mp4
```

`session.json` logs exo's reasoning + buttons per turn, so you can narrate _why_
it pressed what.

## Status / notes

- **Validated:** the vision→buttons loop is verified against exo+gpt-5.5 (a static
  test frame in, a valid `{"buttons":[...]}` decision out). A full play session
  needs your ROM.
- From a fresh boot the agent must navigate the title/new-game screens by sight
  (slow). Provide a `POKEMON_STATE` to start in the overworld for a faster, more
  legible demo.
- Next levers: durable **memory** (remember badges/locations/party so progress
  survives a long session), a periodic state summary to bound context, and a
  reward/eval signal (badges earned, map locations reached).
