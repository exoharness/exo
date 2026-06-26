import { describe, expect, it } from "vitest";

import {
  buildIdeogramInput,
  extractFalImages,
  generateFalImage,
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
});
