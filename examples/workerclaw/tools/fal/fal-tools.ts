import { randomUUID } from "node:crypto";
import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

import {
  type HarnessToolRegistry,
  type JsonObject,
  type JsonValue,
  type ToolResult,
} from "@exo/harness";

const IDEOGRAM_V4_MODEL = "ideogram/v4";
const IDEOGRAM_V4_ENDPOINT = "https://fal.run/ideogram/v4";
const DEFAULT_TIMEOUT_MS = 120_000;
const DEFAULT_CACHE_ROOT = "/tmp/exoclaw-fal";
const DEFAULT_CACHE_MOUNT = "/fal";
const MAX_FAL_IMAGE_BYTES = 8 * 1024 * 1024;

type FalImage = {
  url: string;
  content_type?: string;
  file_name?: string;
  file_size?: number;
  width?: number;
  height?: number;
};

type AttachedImage = {
  hostPath: string;
  sandboxPath: string;
  mimeType: string;
  fileName: string;
  sizeBytes: number;
};

type ResultImage = {
  url: string | null;
  mimeType: string | null;
  fileName: string | null;
  fileSize: number | null;
  width: number | null;
  height: number | null;
  path?: string;
  sandboxPath?: string;
};

export interface GenerateFalImageOptions {
  apiKey?: string;
  callFal?: (input: JsonObject, apiKey: string) => Promise<unknown>;
  fetchImage?: (
    url: string,
    fallbackMimeType: string,
  ) => Promise<AttachedImage | null>;
}

export function registerFalTools(registry: HarnessToolRegistry): void {
  registry.register({
    source: "built_in",
    definition: {
      name: "fal_generate_image",
      description:
        "Generate images with Fal Ideogram 4.0 (`ideogram/v4`). Requires FAL_KEY in the host environment. Returns locally cached sandbox paths that can be passed directly to send_adapter_message attachments (kind=image, sandboxPath=...). By default this does not attach image bytes into the conversation; set attachToConversation=true only when you need to inspect the first image visually in the next model round.",
      parameters: {
        type: "object",
        additionalProperties: false,
        properties: {
          prompt: {
            type: "string",
            description: "Prompt to generate an image from.",
          },
          numImages: {
            type: ["number", "null"],
            description:
              "Number of images to generate, 1-4. Null defaults to 1.",
          },
          imageSize: {
            anyOf: [
              {
                type: "string",
                enum: [
                  "square_hd",
                  "square",
                  "portrait_4_3",
                  "portrait_16_9",
                  "landscape_4_3",
                  "landscape_16_9",
                ],
              },
              {
                type: "object",
                additionalProperties: false,
                properties: {
                  width: { type: "number" },
                  height: { type: "number" },
                },
                required: ["width", "height"],
              },
              { type: "null" },
            ],
            description:
              "Fal image_size. Null defaults to square_hd. Can be an enum or {width,height}.",
          },
          renderingSpeed: {
            type: ["string", "null"],
            enum: ["TURBO", "BALANCED", "QUALITY", null],
            description: "Fal rendering_speed. Null defaults to BALANCED.",
          },
          acceleration: {
            type: ["string", "null"],
            enum: ["none", "low", "regular", "high", null],
            description: "Fal acceleration. Null defaults to none.",
          },
          expansionModel: {
            type: ["string", "null"],
            enum: ["None", "Medium", "Large", null],
            description:
              "Prompt expansion model. None disables expansion, Medium is fast, Large uses Magic Prompt.",
          },
          outputFormat: {
            type: ["string", "null"],
            enum: ["jpeg", "png", null],
            description: "Output format. Null defaults to jpeg.",
          },
          seed: {
            type: ["number", "null"],
            description: "Optional integer seed. Null lets Fal choose one.",
          },
          enableSafetyChecker: {
            type: ["boolean", "null"],
            description:
              "Whether to enable Fal's safety checker. Null uses Fal's default.",
          },
          attachToConversation: {
            type: ["boolean", "null"],
            description:
              "If true, fetch and attach the first generated image back into this conversation for visual inspection. Null/false returns URLs only, which is best when posting to Discord.",
          },
        },
        required: [
          "prompt",
          "numImages",
          "imageSize",
          "renderingSpeed",
          "acceleration",
          "expansionModel",
          "outputFormat",
          "seed",
          "enableSafetyChecker",
          "attachToConversation",
        ],
      },
    },
    handler: {
      async execute(args): Promise<ToolResult> {
        return generateFalImage(args, { apiKey: process.env.FAL_KEY });
      },
    },
  });
}

export async function generateFalImage(
  args: JsonObject,
  options: GenerateFalImageOptions = {},
): Promise<ToolResult> {
  const apiKey = options.apiKey;
  if (!apiKey) {
    return {
      ok: false,
      error: "FAL_KEY is not set in the host environment.",
    };
  }

  let input: JsonObject;
  try {
    input = buildIdeogramInput(args);
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }

  const callFal = options.callFal ?? callFalIdeogramV4;
  let response: unknown;
  try {
    response = await callFal(input, apiKey);
  } catch (error) {
    return {
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    };
  }

  const images = extractFalImages(response);
  if (images.length === 0) {
    return {
      ok: false,
      error: "Fal response did not include any images.",
      response: toJsonValue(response),
    };
  }

  const resultImages: ResultImage[] = images.map((image) => ({
    url: image.url.startsWith("https://") ? image.url : null,
    mimeType: image.content_type ?? null,
    fileName: image.file_name ?? null,
    fileSize: image.file_size ?? null,
    width: image.width ?? null,
    height: image.height ?? null,
  }));

  const result: JsonObject = {
    ok: true,
    model: IDEOGRAM_V4_MODEL,
    prompt:
      typeof (response as { prompt?: unknown }).prompt === "string"
        ? (response as { prompt: string }).prompt
        : input.prompt,
    seed:
      typeof (response as { seed?: unknown }).seed === "number"
        ? (response as { seed: number }).seed
        : null,
    images: resultImages,
    note: "Use images[0].sandboxPath as a send_adapter_message attachment sandboxPath to post this image externally. Set attachToConversation=true only when you need the first image attached back into the model context.",
  };

  const fetchImage = options.fetchImage ?? cacheFalImage;
  const cachedImages: AttachedImage[] = [];
  for (let index = 0; index < images.length; index += 1) {
    const image = images[index];
    if (!image) {
      continue;
    }
    let cached: AttachedImage | null;
    try {
      cached = await fetchImage(
        image.url,
        image.content_type ?? mimeTypeForUrl(image.url),
      );
    } catch {
      cached = null;
    }
    if (!cached) {
      continue;
    }
    cachedImages.push(cached);
    resultImages[index] = {
      ...resultImages[index],
      path: cached.hostPath,
      sandboxPath: cached.sandboxPath,
      mimeType: cached.mimeType,
      fileName: cached.fileName,
      fileSize: cached.sizeBytes,
    };
  }

  if (args.attachToConversation === true && cachedImages[0]) {
    result.media = [
      {
        type: "image",
        path: cachedImages[0].hostPath,
        mimeType: cachedImages[0].mimeType,
      },
    ];
  }

  return result;
}

export function buildIdeogramInput(args: JsonObject): JsonObject {
  const prompt = args.prompt;
  if (typeof prompt !== "string" || prompt.trim().length === 0) {
    throw new Error("prompt must be a non-empty string");
  }

  const input: JsonObject = { prompt };
  // Prefer data-return mode and cache locally. Fal-hosted URLs can expire or
  // occasionally 404 before the Discord adapter fetches them.
  input.sync_mode = true;
  const numImages = nullableNumber(args.numImages);
  if (numImages !== null) {
    if (!Number.isInteger(numImages) || numImages < 1 || numImages > 4) {
      throw new Error("numImages must be an integer from 1 to 4");
    }
    input.num_images = numImages;
  }

  if (args.imageSize !== null && args.imageSize !== undefined) {
    input.image_size = args.imageSize;
  }
  setOptionalString(input, "rendering_speed", args.renderingSpeed);
  setOptionalString(input, "acceleration", args.acceleration);
  setOptionalString(input, "expansion_model", args.expansionModel);
  setOptionalString(input, "output_format", args.outputFormat);

  const seed = nullableNumber(args.seed);
  if (seed !== null) {
    if (!Number.isInteger(seed)) {
      throw new Error("seed must be an integer");
    }
    input.seed = seed;
  }

  if (typeof args.enableSafetyChecker === "boolean") {
    input.enable_safety_checker = args.enableSafetyChecker;
  }

  return input;
}

export function extractFalImages(response: unknown): FalImage[] {
  if (!response || typeof response !== "object" || Array.isArray(response)) {
    return [];
  }
  const images = (response as { images?: unknown }).images;
  if (!Array.isArray(images)) {
    return [];
  }
  return images.flatMap((image): FalImage[] => {
    if (!image || typeof image !== "object" || Array.isArray(image)) {
      return [];
    }
    const candidate = image as Record<string, unknown>;
    if (typeof candidate.url !== "string" || candidate.url.length === 0) {
      return [];
    }
    return [
      {
        url: candidate.url,
        content_type:
          typeof candidate.content_type === "string"
            ? candidate.content_type
            : undefined,
        file_name:
          typeof candidate.file_name === "string"
            ? candidate.file_name
            : undefined,
        file_size:
          typeof candidate.file_size === "number"
            ? candidate.file_size
            : undefined,
        width:
          typeof candidate.width === "number" ? candidate.width : undefined,
        height:
          typeof candidate.height === "number" ? candidate.height : undefined,
      },
    ];
  });
}

async function callFalIdeogramV4(
  input: JsonObject,
  apiKey: string,
): Promise<unknown> {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), DEFAULT_TIMEOUT_MS);
  try {
    const response = await fetch(IDEOGRAM_V4_ENDPOINT, {
      method: "POST",
      headers: {
        Authorization: `Key ${apiKey}`,
        "Content-Type": "application/json",
      },
      body: JSON.stringify(input),
      signal: controller.signal,
    });
    if (!response.ok) {
      const body = await response.text();
      throw new Error(
        `Fal request failed (${response.status}): ${body.slice(0, 500)}`,
      );
    }
    return response.json();
  } finally {
    clearTimeout(timeout);
  }
}

async function cacheFalImage(
  url: string,
  fallbackMimeType: string,
): Promise<AttachedImage | null> {
  const { bytes, mimeType } = url.startsWith("data:")
    ? decodeDataUrl(url, fallbackMimeType)
    : await fetchImageBytes(url, fallbackMimeType);
  if (bytes.byteLength > MAX_FAL_IMAGE_BYTES) {
    return null;
  }
  const extension = extensionForMimeType(mimeType);
  const fileName = `${Date.now()}-${randomUUID()}${extension}`;
  const root = falCacheRoot();
  await mkdir(root, { recursive: true });
  const hostPath = path.join(root, fileName);
  await writeFile(hostPath, bytes);
  return {
    hostPath,
    sandboxPath: `${falCacheMount()}/${fileName}`,
    mimeType,
    fileName,
    sizeBytes: bytes.byteLength,
  };
}

async function fetchImageBytes(
  url: string,
  fallbackMimeType: string,
): Promise<{ bytes: Uint8Array; mimeType: string }> {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`Fal image fetch failed (${response.status}) for ${url}`);
  }
  const contentLength = response.headers.get("content-length");
  if (contentLength && Number(contentLength) > MAX_FAL_IMAGE_BYTES) {
    throw new Error(`Fal image exceeds ${MAX_FAL_IMAGE_BYTES} bytes: ${url}`);
  }
  return {
    bytes: new Uint8Array(await response.arrayBuffer()),
    mimeType: response.headers.get("content-type") ?? fallbackMimeType,
  };
}

function decodeDataUrl(
  dataUrl: string,
  fallbackMimeType: string,
): { bytes: Uint8Array; mimeType: string } {
  const comma = dataUrl.indexOf(",");
  if (comma === -1) {
    throw new Error("Fal returned an invalid data URL");
  }
  const header = dataUrl.slice(5, comma);
  const mimeType = header.split(";")[0] || fallbackMimeType;
  const payload = dataUrl.slice(comma + 1);
  return {
    bytes: Buffer.from(payload, "base64"),
    mimeType,
  };
}

function nullableNumber(value: JsonValue | undefined): number | null {
  return typeof value === "number" ? value : null;
}

function setOptionalString(
  input: JsonObject,
  key: string,
  value: JsonValue | undefined,
): void {
  if (typeof value === "string") {
    input[key] = value;
  }
}

function mimeTypeForUrl(url: string): string {
  if (url.startsWith("data:")) {
    const comma = url.indexOf(",");
    const header = comma === -1 ? url.slice(5) : url.slice(5, comma);
    const mimeType = header.split(";")[0];
    return mimeType.startsWith("image/") ? mimeType : "image/png";
  }
  const path = new URL(url).pathname.toLowerCase();
  if (path.endsWith(".png")) {
    return "image/png";
  }
  if (path.endsWith(".webp")) {
    return "image/webp";
  }
  return "image/jpeg";
}

function extensionForMimeType(mimeType: string): string {
  switch (mimeType.split(";")[0]) {
    case "image/png":
      return ".png";
    case "image/webp":
      return ".webp";
    case "image/gif":
      return ".gif";
    default:
      return ".jpg";
  }
}

function falCacheRoot(): string {
  return process.env.EXOCLAW_FAL_IMAGE_CACHE ?? DEFAULT_CACHE_ROOT;
}

function falCacheMount(): string {
  return process.env.EXOCLAW_FAL_IMAGE_MOUNT ?? DEFAULT_CACHE_MOUNT;
}

function toJsonValue(value: unknown): JsonValue {
  return JSON.parse(JSON.stringify(value)) as JsonValue;
}
