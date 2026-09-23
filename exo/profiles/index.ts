import { bootstrapProfile } from "./bootstrap";
import { memoryOnlyProfile } from "./memory-only";
import { practicalProfile } from "./practical";
import type { ExoProfile, ExoProfileName } from "./types";

const PROFILES: Record<ExoProfileName, ExoProfile> = {
  bootstrap: bootstrapProfile,
  practical: practicalProfile,
  "memory-only": memoryOnlyProfile,
};

export function resolveExoProfile(name = process.env.EXO_PROFILE): ExoProfile {
  const profileName = name ?? "practical";
  if (!isProfileName(profileName)) {
    throw new Error(
      `unknown EXO_PROFILE ${JSON.stringify(profileName)}; expected one of ${Object.keys(PROFILES).join(", ")}`,
    );
  }
  return PROFILES[profileName];
}

export type { ExoProfile, ExoProfileName } from "./types";

function isProfileName(name: string): name is ExoProfileName {
  return Object.hasOwn(PROFILES, name);
}
