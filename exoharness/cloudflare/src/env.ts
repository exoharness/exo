import type { ExoProvider } from "./provider";
import type { ExoSandbox } from "./sandbox";

export interface Env {
  PROVIDERS: DurableObjectNamespace<ExoProvider>;
  SANDBOXES: DurableObjectNamespace<ExoSandbox>;
  ARTIFACTS: R2Bucket;
  ACCOUNT_ID: string;
  EXO_TOKEN?: string;
  // Set to require the platform-verified Cloudflare Access application audience.
  ACCESS_AUD?: string;
  VAULT_KEY: string;
  // Optional test receiver: responds with booleans, never reflects the key.
  PROBE_KEY?: string;
}

export interface SandboxIdentity {
  agentId: string;
  threadId: string;
}
export interface SandboxPolicy {
  origins: string[];
  credentials: {
    environmentVariable: string;
    credential: string;
    placeholder: string;
  }[];
}
export interface ExecRequest {
  command: string[];
  env?: Record<string, string>;
  timeoutMs?: number;
}
export interface ExecResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}
