import { afterEach, describe, expect, it } from "vitest";

import {
  adapterConfig,
  booleanField,
  numberField,
  nullableStringValue,
  optionalStringField,
  parseAttachments,
  parseWorkerCommand,
  stringField,
} from "./protocol";

describe("parseWorkerCommand", () => {
  it("parses a valid send_message command", () => {
    expect(
      parseWorkerCommand({
        type: "send_message",
        id: "cmd-1",
        target: "#exo",
        text: "hello",
      }),
    ).toEqual({
      type: "send_message",
      id: "cmd-1",
      target: "#exo",
      text: "hello",
      attachments: [],
    });
  });

  it("defaults a missing target to null", () => {
    expect(
      parseWorkerCommand({ type: "send_message", id: "cmd-1", text: "hi" })
        .target,
    ).toBeNull();
  });

  it("rejects empty id, text, and target", () => {
    expect(() =>
      parseWorkerCommand({ type: "send_message", id: "", text: "hi" }),
    ).toThrow("send_message id must be a non-empty string");
    expect(() =>
      parseWorkerCommand({ type: "send_message", id: "cmd-1", text: "" }),
    ).toThrow("send_message text must be a non-empty string");
    expect(() =>
      parseWorkerCommand({
        type: "send_message",
        id: "cmd-1",
        target: "",
        text: "hi",
      }),
    ).toThrow("send_message target must be null or a non-empty string");
  });

  it("rejects unknown commands and non-objects", () => {
    expect(() =>
      parseWorkerCommand({ type: "delete_message", id: "x" }),
    ).toThrow("worker command must be a send_message object");
    expect(() => parseWorkerCommand("send_message")).toThrow(
      "worker command must be a send_message object",
    );
    expect(() => parseWorkerCommand(null)).toThrow(
      "worker command must be a send_message object",
    );
  });
});

describe("parseAttachments", () => {
  it("treats null or undefined as no attachments", () => {
    expect(parseAttachments(null)).toEqual([]);
    expect(parseAttachments(undefined)).toEqual([]);
  });

  it("parses a valid attachment and normalizes optional fields to null", () => {
    expect(
      parseAttachments([{ kind: "image", url: "https://cdn.example/a.png" }]),
    ).toEqual([
      {
        kind: "image",
        path: null,
        url: "https://cdn.example/a.png",
        data: null,
        mimeType: null,
        fileName: null,
      },
    ]);
  });

  it("enforces exactly one of path, url, or data", () => {
    expect(() => parseAttachments([{ kind: "image" }])).toThrow(
      "send_message attachment must specify exactly one of path, url, or data",
    );
    expect(() =>
      parseAttachments([
        { kind: "image", path: "/tmp/a.png", url: "https://cdn.example/a.png" },
      ]),
    ).toThrow(
      "send_message attachment must specify exactly one of path, url, or data",
    );
  });

  it("rejects non-https urls", () => {
    expect(() =>
      parseAttachments([{ kind: "image", url: "http://cdn.example/a.png" }]),
    ).toThrow("send_message attachment url must use https");
  });

  it("validates the kind enum", () => {
    for (const kind of ["image", "video", "audio", "document"]) {
      expect(parseAttachments([{ kind, path: "/tmp/x" }])[0].kind).toBe(kind);
    }
    expect(() =>
      parseAttachments([{ kind: "sticker", path: "/tmp/x" }]),
    ).toThrow(
      "send_message attachment kind must be image, video, audio, or document",
    );
  });

  it("rejects non-array and non-object shapes", () => {
    expect(() => parseAttachments("nope")).toThrow(
      "send_message attachments must be null or an array",
    );
    expect(() => parseAttachments(["nope"])).toThrow(
      "send_message attachment must be an object",
    );
  });
});

describe("config field validators", () => {
  it("stringField requires a non-empty string", () => {
    expect(stringField({ token: "abc" }, "token")).toBe("abc");
    expect(() => stringField({ token: "" }, "token")).toThrow(
      "adapter config token must be a non-empty string",
    );
    expect(() => stringField({}, "token")).toThrow(
      "adapter config token must be a non-empty string",
    );
  });

  it("optionalStringField allows null/undefined but not empty strings", () => {
    expect(optionalStringField({ nick: "exo" }, "nick")).toBe("exo");
    expect(optionalStringField({ nick: null }, "nick")).toBeNull();
    expect(optionalStringField({}, "nick")).toBeNull();
    expect(() => optionalStringField({ nick: "" }, "nick")).toThrow(
      "adapter config nick must be null or a non-empty string",
    );
  });

  it("booleanField requires a boolean", () => {
    expect(booleanField({ tls: true }, "tls")).toBe(true);
    expect(booleanField({ tls: false }, "tls")).toBe(false);
    expect(() => booleanField({ tls: "true" }, "tls")).toThrow(
      "adapter config tls must be a boolean",
    );
    expect(() => booleanField({}, "tls")).toThrow(
      "adapter config tls must be a boolean",
    );
  });

  it("numberField requires a number", () => {
    expect(numberField({ port: 6697 }, "port")).toBe(6697);
    expect(() => numberField({ port: "6697" }, "port")).toThrow(
      "adapter config port must be a number",
    );
    expect(() => numberField({}, "port")).toThrow(
      "adapter config port must be a number",
    );
  });

  it("nullableStringValue allows null/undefined but rejects empties and non-strings", () => {
    expect(nullableStringValue("value", "attachment path")).toBe("value");
    expect(nullableStringValue(null, "attachment path")).toBeNull();
    expect(nullableStringValue(undefined, "attachment path")).toBeNull();
    expect(() => nullableStringValue("", "attachment path")).toThrow(
      "attachment path must be null or a non-empty string",
    );
    expect(() => nullableStringValue(42, "attachment path")).toThrow(
      "attachment path must be null or a non-empty string",
    );
  });
});

describe("adapterConfig", () => {
  const original = process.env.EXO_ADAPTER_CONFIG;

  afterEach(() => {
    if (original === undefined) {
      delete process.env.EXO_ADAPTER_CONFIG;
    } else {
      process.env.EXO_ADAPTER_CONFIG = original;
    }
  });

  it("returns an empty object when the variable is unset", () => {
    delete process.env.EXO_ADAPTER_CONFIG;
    expect(adapterConfig()).toEqual({});
  });

  it("parses a JSON object", () => {
    process.env.EXO_ADAPTER_CONFIG = '{"server":"irc.libera.chat","port":6697}';
    expect(adapterConfig()).toEqual({ server: "irc.libera.chat", port: 6697 });
  });

  it("throws when the variable holds JSON that is not an object", () => {
    process.env.EXO_ADAPTER_CONFIG = "[1,2,3]";
    expect(() => adapterConfig()).toThrow(
      "EXO_ADAPTER_CONFIG must contain a JSON object",
    );
    process.env.EXO_ADAPTER_CONFIG = '"just a string"';
    expect(() => adapterConfig()).toThrow(
      "EXO_ADAPTER_CONFIG must contain a JSON object",
    );
  });
});
