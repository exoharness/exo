import { describe, expect, it } from "vitest";

import { pcmToWav } from "./voice";

// Discord voice constants baked into the header: 48kHz, stereo, s16le.
const SAMPLE_RATE = 48_000;
const CHANNELS = 2;
const BYTES_PER_SAMPLE = 2;

describe("pcmToWav", () => {
  it("wraps PCM in a 44-byte RIFF/WAVE header with correct chunk fields", () => {
    const pcm = Buffer.from([1, 2, 3, 4, 5, 6, 7, 8]);
    const wav = pcmToWav(pcm);

    expect(wav.length).toBe(44 + pcm.length);
    expect(wav.toString("ascii", 0, 4)).toBe("RIFF");
    // RIFF chunk size is the whole file minus the 8-byte RIFF header.
    expect(wav.readUInt32LE(4)).toBe(36 + pcm.length);
    expect(wav.toString("ascii", 8, 12)).toBe("WAVE");

    expect(wav.toString("ascii", 12, 16)).toBe("fmt ");
    expect(wav.readUInt32LE(16)).toBe(16); // PCM fmt chunk size
    expect(wav.readUInt16LE(20)).toBe(1); // audio format = PCM
    expect(wav.readUInt16LE(22)).toBe(CHANNELS);
    expect(wav.readUInt32LE(24)).toBe(SAMPLE_RATE);
    expect(wav.readUInt32LE(28)).toBe(
      SAMPLE_RATE * CHANNELS * BYTES_PER_SAMPLE,
    );
    expect(wav.readUInt16LE(32)).toBe(CHANNELS * BYTES_PER_SAMPLE);
    expect(wav.readUInt16LE(34)).toBe(BYTES_PER_SAMPLE * 8);

    expect(wav.toString("ascii", 36, 40)).toBe("data");
    expect(wav.readUInt32LE(40)).toBe(pcm.length);
    expect(wav.subarray(44)).toEqual(pcm);
  });

  it("produces a bare header for empty PCM", () => {
    const wav = pcmToWav(Buffer.alloc(0));
    expect(wav.length).toBe(44);
    expect(wav.readUInt32LE(4)).toBe(36);
    expect(wav.readUInt32LE(40)).toBe(0);
  });
});
