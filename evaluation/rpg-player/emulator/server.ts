// EmulatorJS sidecar: exposes the same HTTP JSON contract as the PyBoy
// sidecar in evaluation/pokemon-gameplay, but hosts the emulator in headless
// Chromium (Playwright) running EmulatorJS (RetroArch cores compiled to
// WASM). One sidecar, many consoles — the default core is segaMS for
// Phantasy Star 1 on the Sega Master System.
//
//   pnpm exec tsx emulator/server.ts --rom roms/phantasy-star.sms
//
// Args: --rom <path> (required), --port <n> (default 8777),
//       --core <name> (default segaMS), --headed (watch live).
// Env:  RPG_EJS_DATA_URL overrides the pinned EmulatorJS CDN data dir.
//
// API (all JSON):
//   GET  /health            -> { ok, rom, core }
//   GET  /frame             -> FramePayload
//   POST /press             -> { buttons, hold_frames?, wait_frames? }
//   POST /tick              -> { frames }
//   POST /checkpoint/save   -> { name }
//   POST /checkpoint/load   -> { name }
//   GET  /checkpoints       -> { checkpoints: [names] }
//   POST /reset             -> {}
//
// The emulator is PAUSED between requests: the world only moves when the
// agent acts, same as the PyBoy harness.

import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { chromium, type Browser, type Page } from "playwright";

const EMULATOR_DIR = path.dirname(fileURLToPath(import.meta.url));
const BASE_DIR = path.resolve(EMULATOR_DIR, "..");
const RUNTIME_DIR =
  process.env.RPG_RUNTIME_DIR !== undefined &&
  process.env.RPG_RUNTIME_DIR.length > 0
    ? path.resolve(process.env.RPG_RUNTIME_DIR)
    : path.join(BASE_DIR, "runtime");
const CHECKPOINT_DIR = path.join(RUNTIME_DIR, "checkpoints");
const EJS_DATA_URL =
  process.env.RPG_EJS_DATA_URL ?? "https://cdn.emulatorjs.org/4.2.3/data/";

const FPS = 60; // SMS/NES/Genesis all run at ~60Hz
const DEFAULT_HOLD_FRAMES = 8;
const DEFAULT_WAIT_FRAMES = 40;
const MAX_HOLD_FRAMES = 120;
const MAX_WAIT_FRAMES = 600;
const MAX_TICK_FRAMES = 3600;
const CHECKPOINT_NAME_RE = /^[A-Za-z0-9_.-]{1,64}$/;

// RetroArch joypad ids (EmulatorJS simulateInput uses these). genesis_plus_gx
// maps the SMS pad as: Button 1 -> RetroPad B (0), Button 2 -> RetroPad A (8),
// console Pause -> RetroPad Start (3).
const BUTTON_IDS: Record<string, number> = {
  up: 4,
  down: 5,
  left: 6,
  right: 7,
  button1: 0,
  button2: 8,
  pause: 3,
  // Aliases so agents used to A/B/start naming still work.
  a: 8,
  b: 0,
  start: 3,
  select: 2,
};

interface CliOptions {
  rom: string;
  port: number;
  core: string;
  headed: boolean;
}

function parseArgs(argv: string[]): CliOptions {
  const options: CliOptions = {
    rom: "",
    port: Number(process.env.RPG_EMULATOR_PORT ?? "8777"),
    core: "segaMS",
    headed: process.env.RPG_HEADED === "1",
  };
  for (let i = 0; i < argv.length; i += 1) {
    switch (argv[i]) {
      case "--rom":
        options.rom = argv[++i] ?? "";
        break;
      case "--port":
        options.port = Number(argv[++i] ?? "8777");
        break;
      case "--core":
        options.core = argv[++i] ?? "segaMS";
        break;
      case "--headed":
        options.headed = true;
        break;
      default:
        throw new Error(`unknown argument: ${argv[i]}`);
    }
  }
  if (options.rom.length === 0 || !fs.existsSync(options.rom)) {
    throw new Error("--rom <path> is required and must exist");
  }
  return options;
}

class Emulator {
  private browser: Browser | null = null;
  private page: Page | null = null;
  private frameCount = 0;

  constructor(private readonly options: CliOptions) {}

  async start(serverPort: number): Promise<void> {
    this.browser = await chromium.launch({
      headless: !this.options.headed,
      args: [
        // Headed runs are for watching, so keep the soundtrack; headless has
        // nobody listening and muting sidesteps WebAudio autoplay quirks.
        ...(this.options.headed ? [] : ["--mute-audio"]),
        "--autoplay-policy=no-user-gesture-required",
        // WebGL without a GPU in headless environments.
        "--enable-unsafe-swiftshader",
      ],
    });
    this.page = await this.browser.newPage({
      viewport: { width: 800, height: 620 },
    });
    this.page.on("console", (message) => {
      if (message.type() === "error") {
        console.error(`[page] ${message.text()}`);
      }
    });
    const dataParam = encodeURIComponent(EJS_DATA_URL);
    const coreParam = encodeURIComponent(this.options.core);
    const audioParam = this.options.headed ? "1" : "0";
    await this.page.goto(
      `http://127.0.0.1:${serverPort}/page/index.html?core=${coreParam}&data=${dataParam}&audio=${audioParam}`,
    );
    await this.page.waitForFunction("window.__gameStarted === true", null, {
      timeout: 120_000,
    });
    // Let the core render its first frames, then adopt the paused-between-
    // requests convention.
    await this.page.waitForTimeout(1_000);
    await this.setRunning(false);
    console.log("emulator booted and paused");
  }

  async stop(): Promise<void> {
    await this.browser?.close();
  }

  private requirePage(): Page {
    if (this.page === null) {
      throw new Error("emulator page is not running");
    }
    return this.page;
  }

  private async setRunning(running: boolean): Promise<void> {
    await this.requirePage().evaluate((play) => {
      const emulator = (
        window as unknown as {
          EJS_emulator: {
            play(): void;
            pause(): void;
          };
        }
      ).EJS_emulator;
      if (play) {
        emulator.play();
      } else {
        emulator.pause();
      }
    }, running);
  }

  // Runs the core for approximately `frames` frames by unpausing for the
  // equivalent wall-clock time. EmulatorJS has no public frame-step API, so
  // time-based advancement is the contract here; determinism comes from the
  // pause-between-requests convention, not exact frame counts.
  private async runFrames(frames: number): Promise<void> {
    if (frames <= 0) {
      return;
    }
    await this.setRunning(true);
    await this.requirePage().waitForTimeout(Math.ceil((frames * 1_000) / FPS));
    await this.setRunning(false);
    this.frameCount += frames;
  }

  private async holdButton(id: number, frames: number): Promise<void> {
    const page = this.requirePage();
    await this.setRunning(true);
    try {
      await page.evaluate(
        async ([button, holdMs]) => {
          const emulator = (
            window as unknown as {
              EJS_emulator: {
                gameManager: {
                  simulateInput(
                    player: number,
                    index: number,
                    value: number,
                  ): void;
                };
              };
            }
          ).EJS_emulator;
          emulator.gameManager.simulateInput(0, button, 1);
          await new Promise((resolve) => setTimeout(resolve, holdMs));
          emulator.gameManager.simulateInput(0, button, 0);
        },
        [id, Math.ceil((frames * 1_000) / FPS)] as const,
      );
    } finally {
      await this.setRunning(false);
    }
    this.frameCount += frames;
  }

  async press(
    buttons: string[],
    holdFrames: number,
    waitFrames: number,
  ): Promise<void> {
    for (const button of buttons) {
      const id = BUTTON_IDS[button];
      if (id === undefined) {
        throw new Error(
          `unknown button '${button}' (valid: ${Object.keys(BUTTON_IDS).join(", ")})`,
        );
      }
      await this.holdButton(id, holdFrames);
      await this.runFrames(waitFrames);
    }
  }

  async tick(frames: number): Promise<void> {
    await this.runFrames(frames);
  }

  async frame(): Promise<{
    screenshot_b64: string;
    state: Record<string, unknown>;
    screen_hash: string;
    frame_count: number;
  }> {
    const page = this.requirePage();
    const canvas = page.locator("#game canvas").first();
    const png = await canvas.screenshot({ type: "png" });
    return {
      screenshot_b64: png.toString("base64"),
      // No structured RAM state through EmulatorJS (yet) — a per-game probe
      // can populate this later; vision is the primary channel.
      state: {},
      screen_hash: crypto
        .createHash("sha1")
        .update(png)
        .digest("hex")
        .slice(0, 16),
      frame_count: this.frameCount,
    };
  }

  async saveCheckpoint(name: string): Promise<void> {
    const stateB64 = await this.requirePage().evaluate(async () => {
      const emulator = (
        window as unknown as {
          EJS_emulator: {
            gameManager: { getState(): Uint8Array | Promise<Uint8Array> };
          };
        }
      ).EJS_emulator;
      const state = await emulator.gameManager.getState();
      let binary = "";
      const chunk = 0x8000;
      for (let i = 0; i < state.length; i += chunk) {
        binary += String.fromCharCode(...state.subarray(i, i + chunk));
      }
      return btoa(binary);
    });
    fs.mkdirSync(CHECKPOINT_DIR, { recursive: true });
    fs.writeFileSync(
      path.join(CHECKPOINT_DIR, `${name}.state`),
      Buffer.from(stateB64, "base64"),
    );
  }

  async loadCheckpoint(name: string): Promise<void> {
    const statePath = path.join(CHECKPOINT_DIR, `${name}.state`);
    if (!fs.existsSync(statePath)) {
      throw new Error(`no checkpoint named '${name}'`);
    }
    const stateB64 = fs.readFileSync(statePath).toString("base64");
    await this.requirePage().evaluate(async (b64) => {
      const emulator = (
        window as unknown as {
          EJS_emulator: {
            gameManager: {
              loadState(state: Uint8Array): void | Promise<void>;
            };
          };
        }
      ).EJS_emulator;
      const binary = atob(b64);
      const state = new Uint8Array(binary.length);
      for (let i = 0; i < binary.length; i += 1) {
        state[i] = binary.charCodeAt(i);
      }
      await emulator.gameManager.loadState(state);
    }, stateB64);
    // Let the loaded state render a frame so the next screenshot is current.
    await this.runFrames(5);
  }

  async reset(): Promise<void> {
    await this.requirePage().evaluate(() => {
      const emulator = (
        window as unknown as {
          EJS_emulator: { gameManager: { restart(): void } };
        }
      ).EJS_emulator;
      emulator.gameManager.restart();
    });
    await this.runFrames(30);
  }
}

function clampNumber(
  value: unknown,
  fallback: number,
  min: number,
  max: number,
): number {
  const parsed = typeof value === "number" ? value : Number(value);
  if (!Number.isFinite(parsed)) {
    return fallback;
  }
  return Math.max(min, Math.min(max, Math.round(parsed)));
}

async function readJsonBody(
  request: http.IncomingMessage,
): Promise<Record<string, unknown>> {
  const chunks: Buffer[] = [];
  for await (const chunk of request) {
    chunks.push(chunk as Buffer);
  }
  const raw = Buffer.concat(chunks).toString("utf8");
  if (raw.length === 0) {
    return {};
  }
  const parsed = JSON.parse(raw) as unknown;
  return parsed !== null && typeof parsed === "object"
    ? (parsed as Record<string, unknown>)
    : {};
}

function sendJson(
  response: http.ServerResponse,
  status: number,
  payload: unknown,
): void {
  const body = JSON.stringify(payload);
  response.writeHead(status, {
    "Content-Type": "application/json",
    "Content-Length": Buffer.byteLength(body),
  });
  response.end(body);
}

async function main(): Promise<void> {
  const options = parseArgs(process.argv.slice(2));
  fs.mkdirSync(CHECKPOINT_DIR, { recursive: true });
  const emulator = new Emulator(options);
  let booted = false;

  // Serialize all emulator operations: the page is a single shared resource.
  // The queue starts behind the boot, so API calls arriving while EmulatorJS
  // is still loading simply wait instead of failing.
  let queue: Promise<unknown> = Promise.resolve();
  const enqueue = <T>(task: () => Promise<T>): Promise<T> => {
    const next = queue.then(task, task);
    queue = next.catch(() => {});
    return next;
  };

  const server = http.createServer((request, response) => {
    void (async () => {
      const url = new URL(request.url ?? "/", "http://localhost");
      try {
        // Static assets for the embedded page + the ROM itself.
        if (request.method === "GET" && url.pathname === "/rom") {
          const rom = fs.readFileSync(options.rom);
          response.writeHead(200, {
            "Content-Type": "application/octet-stream",
            "Content-Length": rom.length,
          });
          response.end(rom);
          return;
        }
        if (request.method === "GET" && url.pathname.startsWith("/page/")) {
          const relative = url.pathname.slice("/page/".length);
          const full = path.resolve(EMULATOR_DIR, "page", relative);
          const pageRoot = path.resolve(EMULATOR_DIR, "page");
          if (!full.startsWith(`${pageRoot}${path.sep}`)) {
            sendJson(response, 404, { error: "not found" });
            return;
          }
          const contents = fs.readFileSync(full);
          response.writeHead(200, {
            "Content-Type": full.endsWith(".html")
              ? "text/html"
              : "application/octet-stream",
            "Content-Length": contents.length,
          });
          response.end(contents);
          return;
        }

        if (request.method === "GET" && url.pathname === "/health") {
          sendJson(response, 200, {
            ok: booted,
            rom: path.basename(options.rom),
            core: options.core,
          });
          return;
        }
        if (request.method === "GET" && url.pathname === "/frame") {
          sendJson(response, 200, await enqueue(() => emulator.frame()));
          return;
        }
        if (request.method === "GET" && url.pathname === "/checkpoints") {
          const names = fs
            .readdirSync(CHECKPOINT_DIR)
            .filter((file) => file.endsWith(".state"))
            .map((file) => file.slice(0, -".state".length))
            .sort();
          sendJson(response, 200, { checkpoints: names });
          return;
        }

        if (request.method !== "POST") {
          sendJson(response, 404, { error: "not found" });
          return;
        }
        const body = await readJsonBody(request);

        if (url.pathname === "/press") {
          const buttons = Array.isArray(body.buttons)
            ? body.buttons.map(String)
            : [];
          if (buttons.length === 0 || buttons.length > 20) {
            sendJson(response, 400, { error: "buttons must be 1-20 entries" });
            return;
          }
          const holdFrames = clampNumber(
            body.hold_frames,
            DEFAULT_HOLD_FRAMES,
            1,
            MAX_HOLD_FRAMES,
          );
          const waitFrames = clampNumber(
            body.wait_frames,
            DEFAULT_WAIT_FRAMES,
            0,
            MAX_WAIT_FRAMES,
          );
          const payload = await enqueue(async () => {
            await emulator.press(buttons, holdFrames, waitFrames);
            return await emulator.frame();
          });
          sendJson(response, 200, payload);
          return;
        }
        if (url.pathname === "/tick") {
          const frames = clampNumber(body.frames, 60, 1, MAX_TICK_FRAMES);
          const payload = await enqueue(async () => {
            await emulator.tick(frames);
            return await emulator.frame();
          });
          sendJson(response, 200, payload);
          return;
        }
        if (
          url.pathname === "/checkpoint/save" ||
          url.pathname === "/checkpoint/load"
        ) {
          const name = String(body.name ?? "");
          if (!CHECKPOINT_NAME_RE.test(name)) {
            sendJson(response, 400, {
              error: "name must match [A-Za-z0-9_.-]{1,64}",
            });
            return;
          }
          const payload = await enqueue(async () => {
            if (url.pathname === "/checkpoint/save") {
              await emulator.saveCheckpoint(name);
            } else {
              await emulator.loadCheckpoint(name);
            }
            return await emulator.frame();
          });
          sendJson(response, 200, payload);
          return;
        }
        if (url.pathname === "/reset") {
          const payload = await enqueue(async () => {
            await emulator.reset();
            return await emulator.frame();
          });
          sendJson(response, 200, payload);
          return;
        }
        console.warn(`404 ${request.method} ${url.pathname}`);
        sendJson(response, 404, { error: "not found" });
      } catch (error) {
        sendJson(response, 500, {
          error: error instanceof Error ? error.message : String(error),
        });
      }
    })();
  });

  await new Promise<void>((resolve) =>
    server.listen(options.port, "127.0.0.1", resolve),
  );
  console.log(
    `sidecar listening on http://127.0.0.1:${options.port} (rom=${path.basename(options.rom)} core=${options.core})`,
  );

  const startPromise = emulator.start(options.port).then(() => {
    booted = true;
  });
  // Queue all API work behind the boot.
  queue = startPromise.catch(() => {});
  try {
    await startPromise;
  } catch (error) {
    console.error(
      `emulator failed to boot: ${error instanceof Error ? error.message : String(error)}`,
    );
    process.exit(1);
  }

  const shutdown = () => {
    void emulator.stop().finally(() => process.exit(0));
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

await main();
