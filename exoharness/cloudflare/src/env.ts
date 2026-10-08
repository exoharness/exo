import type { ExoProvider, ExoThread } from "./provider";
import type { ExoSandbox } from "./sandbox";
import type { JsonObject } from "../../typescript/harness/index";
import type { RawResourceScope } from "../../typescript/harness/client";

export interface Env {
  PROVIDERS: DurableObjectNamespace<ExoProvider>;
  THREADS: DurableObjectNamespace<ExoThread>;
  SANDBOXES: DurableObjectNamespace<ExoSandbox>;
  ARTIFACTS: R2Bucket;
  ACCOUNT_ID: string;
  EXO_TOKEN?: string;
  // Set to require the platform-verified Cloudflare Access application audience.
  ACCESS_AUD?: string;
  VAULT_KEY: string;
}

export interface SandboxIdentity {
  agentId: string;
  threadId: string;
  sandboxId: string;
}
export interface SandboxRequest {
  sandbox_id: string;
  scope: RawResourceScope;
  spec: { image: string; default_workdir: string; policy: JsonObject };
  lifecycle: { idle_ttl: { secs: number; nanos: number } | null };
}
export type SandboxPolicy = Record<string, string>;
export interface ExecRequest {
  command: string[];
  env?: Record<string, string>;
  timeoutMs?: number;
  cwd?: string;
}
export interface ExecResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}
