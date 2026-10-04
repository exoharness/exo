import { EventEmitter } from "node:events";
import { afterEach, beforeEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  emit: vi.fn(),
  socket: vi.fn(),
  render: vi.fn(
    (qr: string, _options: unknown, callback: (text: string) => void) =>
      callback(`rendered:${qr}`),
  ),
}));

vi.mock("node:fs/promises", () => ({ default: { mkdir: vi.fn() } }));
vi.mock("node:readline/promises", () => ({
  default: { createInterface: () => [] },
}));
vi.mock("@whiskeysockets/baileys", () => ({
  default: mocks.socket,
  DisconnectReason: { loggedOut: 401 },
  fetchLatestBaileysVersion: async () => ({ version: [1, 0, 0] }),
  useMultiFileAuthState: async () => ({
    state: { creds: { registered: true } },
    saveCreds: vi.fn(),
  }),
}));
vi.mock("qrcode-terminal", () => ({ default: { generate: mocks.render } }));
vi.mock("../protocol", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../protocol")>()),
  writeWorkerEvent: mocks.emit,
}));

let events: EventEmitter;

beforeEach(() => {
  vi.resetModules();
  vi.clearAllMocks();
  vi.stubEnv("EXO_ADAPTER_CONFIG", "{}");
  vi.spyOn(process.stderr, "write").mockReturnValue(true);
  events = new EventEmitter();
  mocks.socket.mockReturnValue({ ev: events, user: { id: "test-user" } });
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllEnvs();
});

it("suppresses repeated codes but logs and emits fresh replacement codes", async () => {
  await import("./worker");
  events.emit("connection.update", { qr: "first" });
  events.emit("connection.update", { qr: "first" });
  events.emit("connection.update", { qr: "replacement" });
  expect(mocks.render.mock.calls.map(([qr]) => qr)).toEqual([
    "first",
    "replacement",
  ]);
  expect(mocks.emit.mock.calls.map(([event]) => event)).toEqual([
    { type: "lifecycle", name: "qr", metadata: { qr: "first" } },
    { type: "lifecycle", name: "qr", metadata: { qr: "replacement" } },
  ]);
  expect(process.stderr.write).toHaveBeenCalledTimes(2);
});

it("allows a new pairing session after connecting", async () => {
  await import("./worker");
  events.emit("connection.update", { qr: "code" });
  events.emit("connection.update", { connection: "open" });
  events.emit("connection.update", { qr: "code" });
  expect(mocks.render).toHaveBeenCalledTimes(2);
  expect(mocks.emit).toHaveBeenCalledWith({
    type: "connected",
    subject: "test-user",
  });
});

it("does not print QR updates for pairing-code linking", async () => {
  vi.stubEnv("EXO_ADAPTER_CONFIG", '{"linkMethod":"pairing-code"}');
  await import("./worker");
  events.emit("connection.update", { qr: "code" });
  expect(mocks.render).not.toHaveBeenCalled();
  expect(mocks.emit).not.toHaveBeenCalled();
});
