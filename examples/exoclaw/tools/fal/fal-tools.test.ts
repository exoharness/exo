import { describe, expect, it } from "vitest";

import {
  buildIdeogramInput,
  decodeDataUrl,
  extensionForMimeType,
  extractFalImages,
  generateFalImage,
  mimeTypeForUrl,
} from "./fal-tools";

describe("buildIdeogramInput", () => {
  it("maps tool args to the Fal Ideogram schema", () => {
    expect(
      buildIdeogramInput({
        prompt: "A poster reading HELLO",
        numImages: 2,
        imageSize: "landscape_16_9",
        renderingSpeed: "QUALITY",
        acceleration: "regular",
        expansionModel: "Large",
        outputFormat: "png",
        seed: 123,
        enableSafetyChecker: true,
        attachToConversation: true,
      }),
    ).toEqual({
      prompt: "A poster reading HELLO",
      sync_mode: true,
      num_images: 2,
      image_size: "landscape_16_9",
      rendering_speed: "QUALITY",
      acceleration: "regular",
      expansion_model: "Large",
      output_format: "png",
      seed: 123,
      enable_safety_checker: true,
    });
  });

  it("omits nullable defaults", () => {
    expect(
      buildIdeogramInput({
        prompt: "A minimal logo",
        numImages: null,
        imageSize: null,
        renderingSpeed: null,
        acceleration: null,
        expansionModel: null,
        outputFormat: null,
        seed: null,
        enableSafetyChecker: null,
        attachToConversation: null,
      }),
    ).toEqual({ prompt: "A minimal logo", sync_mode: true });
  });

  it("validates prompt and image count", () => {
    expect(() => buildIdeogramInput({ prompt: "", numImages: 1 })).toThrow(
      "prompt must be a non-empty string",
    );
    expect(() => buildIdeogramInput({ prompt: "x", numImages: 5 })).toThrow(
      "numImages must be an integer from 1 to 4",
    );
  });
});

describe("extractFalImages", () => {
  it("keeps only image entries with URLs", () => {
    expect(
      extractFalImages({
        images: [
          {
            url: "https://v3b.fal.media/files/a/image.jpg",
            content_type: "image/jpeg",
            file_name: "image.jpg",
            file_size: 123,
            width: 1024,
            height: 768,
          },
          { file_name: "missing-url.jpg" },
          "junk",
        ],
      }),
    ).toEqual([
      {
        url: "https://v3b.fal.media/files/a/image.jpg",
        content_type: "image/jpeg",
        file_name: "image.jpg",
        file_size: 123,
        width: 1024,
        height: 768,
      },
    ]);
  });
});

describe("generateFalImage", () => {
  it("returns cached sandbox paths for sharing without media by default", async () => {
    let capturedInput: unknown;
    const result = await generateFalImage(
      {
        prompt: "A sign reading EXOCLAW",
        numImages: 1,
        imageSize: null,
        renderingSpeed: null,
        acceleration: null,
        expansionModel: null,
        outputFormat: null,
        seed: null,
        enableSafetyChecker: null,
        attachToConversation: null,
      },
      {
        apiKey: "fal-key",
        callFal: async (input, apiKey) => {
          expect(apiKey).toBe("fal-key");
          capturedInput = input;
          return {
            prompt: "expanded prompt",
            seed: 42,
            images: [
              {
                url: "https://v3b.fal.media/files/a/image.jpg",
                content_type: "image/jpeg",
                width: 1024,
                height: 1024,
              },
            ],
          };
        },
        fetchImage: async () => ({
          hostPath: "/tmp/exoclaw-fal/image.jpg",
          sandboxPath: "/fal/image.jpg",
          mimeType: "image/jpeg",
          fileName: "image.jpg",
          sizeBytes: 3,
        }),
      },
    );

    expect(capturedInput).toEqual({
      prompt: "A sign reading EXOCLAW",
      sync_mode: true,
      num_images: 1,
    });
    expect(result).toEqual({
      ok: true,
      model: "ideogram/v4",
      prompt: "expanded prompt",
      seed: 42,
      images: [
        {
          url: "https://v3b.fal.media/files/a/image.jpg",
          mimeType: "image/jpeg",
          fileName: "image.jpg",
          fileSize: 3,
          path: "/tmp/exoclaw-fal/image.jpg",
          sandboxPath: "/fal/image.jpg",
          width: 1024,
          height: 1024,
        },
      ],
      note: "Use images[0].sandboxPath as a send_adapter_message attachment sandboxPath to post this image externally. Set attachToConversation=true only when you need the first image attached back into the model context.",
    });
  });

  it("attaches the first image only when explicitly requested", async () => {
    const result = await generateFalImage(
      {
        prompt: "A sign reading EXOCLAW",
        numImages: null,
        imageSize: null,
        renderingSpeed: null,
        acceleration: null,
        expansionModel: null,
        outputFormat: null,
        seed: null,
        enableSafetyChecker: null,
        attachToConversation: true,
      },
      {
        apiKey: "fal-key",
        callFal: async () => ({
          prompt: "expanded prompt",
          seed: 42,
          images: [
            {
              url: "https://v3b.fal.media/files/a/image.jpg",
              content_type: "image/jpeg",
              width: 1024,
              height: 1024,
            },
          ],
        }),
        fetchImage: async (url, fallbackMimeType) => {
          expect(url).toBe("https://v3b.fal.media/files/a/image.jpg");
          expect(fallbackMimeType).toBe("image/jpeg");
          return {
            hostPath: "/tmp/exoclaw-fal/image.jpg",
            sandboxPath: "/fal/image.jpg",
            mimeType: "image/jpeg",
            fileName: "image.jpg",
            sizeBytes: 3,
          };
        },
      },
    );

    expect(result).toEqual({
      ok: true,
      model: "ideogram/v4",
      prompt: "expanded prompt",
      seed: 42,
      images: [
        {
          url: "https://v3b.fal.media/files/a/image.jpg",
          mimeType: "image/jpeg",
          fileName: "image.jpg",
          fileSize: 3,
          path: "/tmp/exoclaw-fal/image.jpg",
          sandboxPath: "/fal/image.jpg",
          width: 1024,
          height: 1024,
        },
      ],
      note: "Use images[0].sandboxPath as a send_adapter_message attachment sandboxPath to post this image externally. Set attachToConversation=true only when you need the first image attached back into the model context.",
      media: [
        {
          type: "image",
          path: "/tmp/exoclaw-fal/image.jpg",
          mimeType: "image/jpeg",
        },
      ],
    });
  });

  it("fails clearly when FAL_KEY is missing", async () => {
    await expect(generateFalImage({ prompt: "x" })).resolves.toEqual({
      ok: false,
      error: "FAL_KEY is not set in the host environment.",
    });
  });

  it("turns a callFal failure into an error result instead of throwing", async () => {
    await expect(
      generateFalImage(
        { prompt: "x" },
        {
          apiKey: "fal-key",
          callFal: async () => {
            throw new Error("Fal request failed (503): overloaded");
          },
        },
      ),
    ).resolves.toEqual({
      ok: false,
      error: "Fal request failed (503): overloaded",
    });
  });

  it("fails and echoes the response when Fal returns no images", async () => {
    await expect(
      generateFalImage(
        { prompt: "x" },
        {
          apiKey: "fal-key",
          callFal: async () => ({ images: [], detail: "safety filtered" }),
        },
      ),
    ).resolves.toEqual({
      ok: false,
      error: "Fal response did not include any images.",
      response: { images: [], detail: "safety filtered" },
    });
  });

  it("keeps the image without cache paths when fetching it fails", async () => {
    const result = (await generateFalImage(
      {
        prompt: "x",
        attachToConversation: true,
      },
      {
        apiKey: "fal-key",
        callFal: async () => ({
          images: [
            {
              url: "https://v3b.fal.media/files/a/image.jpg",
              content_type: "image/jpeg",
            },
          ],
        }),
        fetchImage: async () => {
          throw new Error("Fal image fetch failed (404)");
        },
      },
    )) as { ok: boolean; images: Record<string, unknown>[]; media?: unknown };

    expect(result.ok).toBe(true);
    expect(result.images).toEqual([
      {
        url: "https://v3b.fal.media/files/a/image.jpg",
        mimeType: "image/jpeg",
        fileName: null,
        fileSize: null,
        width: null,
        height: null,
      },
    ]);
    expect(result.images[0]).not.toHaveProperty("path");
    expect(result.images[0]).not.toHaveProperty("sandboxPath");
    // With nothing cached there is nothing to attach either.
    expect(result.media).toBeUndefined();
  });
});

describe("mimeTypeForUrl", () => {
  it("reads the mime type out of a data URL", () => {
    expect(mimeTypeForUrl("data:image/webp;base64,AAAA")).toBe("image/webp");
  });

  it("falls back to image/png for non-image data URLs", () => {
    expect(mimeTypeForUrl("data:text/plain;base64,AAAA")).toBe("image/png");
  });

  it("maps https URLs by file extension with a jpeg fallback", () => {
    expect(mimeTypeForUrl("https://v3b.fal.media/files/a/image.PNG")).toBe(
      "image/png",
    );
    expect(mimeTypeForUrl("https://v3b.fal.media/files/a/image.webp")).toBe(
      "image/webp",
    );
    expect(mimeTypeForUrl("https://v3b.fal.media/files/a/image.jpg")).toBe(
      "image/jpeg",
    );
    expect(mimeTypeForUrl("https://v3b.fal.media/files/a/image")).toBe(
      "image/jpeg",
    );
  });
});

describe("extensionForMimeType", () => {
  it("maps known image mime types and ignores parameters", () => {
    expect(extensionForMimeType("image/png")).toBe(".png");
    expect(extensionForMimeType("image/webp")).toBe(".webp");
    expect(extensionForMimeType("image/gif")).toBe(".gif");
    expect(extensionForMimeType("image/png;charset=binary")).toBe(".png");
  });

  it("defaults everything else to .jpg", () => {
    expect(extensionForMimeType("image/jpeg")).toBe(".jpg");
    expect(extensionForMimeType("application/octet-stream")).toBe(".jpg");
  });
});

describe("decodeDataUrl", () => {
  it("decodes base64 payloads and keeps the declared mime type", () => {
    const decoded = decodeDataUrl(
      `data:image/png;base64,${Buffer.from("exo").toString("base64")}`,
      "image/jpeg",
    );
    expect(decoded.mimeType).toBe("image/png");
    expect(Buffer.from(decoded.bytes).toString("utf8")).toBe("exo");
  });

  it("uses the fallback mime type when the header omits one", () => {
    const decoded = decodeDataUrl(
      `data:;base64,${Buffer.from("exo").toString("base64")}`,
      "image/jpeg",
    );
    expect(decoded.mimeType).toBe("image/jpeg");
  });

  it("rejects a data URL without a payload separator", () => {
    expect(() => decodeDataUrl("data:image/png;base64", "image/jpeg")).toThrow(
      "Fal returned an invalid data URL",
    );
  });
});
