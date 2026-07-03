import type { ReactNode } from "react";
import type {
  BindingRecord,
  SandboxProcessStatus,
  SecretMetadata,
} from "../api/protocol";
import { formatDateTime, formatJson, shortId } from "../lib/rendering";
import type { MemoryEntry } from "../lib/agentMemory";
import type { SandboxView } from "../lib/sandbox";
import { JsonPreview } from "./JsonPreview";
import { SkeletonRows } from "./SkeletonRows";

export interface ScopedData<T> {
  global: T[];
  agent: T[];
  conversation: T[];
}

type Scope = "global" | "agent" | "conversation";

// Each scope's listing also returns the broader scopes' inherited records, so the
// same secret/binding shows up two or three times. Collapse to one entry per id,
// tagged with the broadest scope it belongs to — that's where it actually lives.
function dedupeByScope<T extends { id: string }>(
  data: ScopedData<T>,
): Array<{ item: T; scope: Scope }> {
  const seen = new Map<string, { item: T; scope: Scope }>();
  const order: Array<[Scope, T[]]> = [
    ["global", data.global],
    ["agent", data.agent],
    ["conversation", data.conversation],
  ];
  for (const [scope, items] of order) {
    for (const item of items) {
      if (!seen.has(item.id)) {
        seen.set(item.id, { item, scope });
      }
    }
  }
  return [...seen.values()];
}

function ScopeTag({ scope }: { scope: Scope }) {
  return <span className={`scope-tag scope-${scope}`}>{scope}</span>;
}

// When every record sits at the same scope, one column of identical tags is just
// noise — return that shared scope so the panel can state it once in its header.
function uniformScope(list: Array<{ scope: Scope }>): Scope | null {
  if (list.length === 0) {
    return null;
  }
  const first = list[0].scope;
  return list.every((entry) => entry.scope === first) ? first : null;
}

interface SidePanelsProps {
  secrets: ScopedData<SecretMetadata>;
  bindings: ScopedData<BindingRecord>;
  sandboxes: SandboxView[];
  memory: MemoryEntry[] | null;
  loading: boolean;
  rollupChips: string[];
  onRefresh: () => void;
}

export function SidePanels({
  secrets,
  bindings,
  sandboxes,
  memory,
  loading,
  rollupChips,
  onRefresh,
}: SidePanelsProps) {
  const secretList = dedupeByScope(secrets);
  const bindingList = dedupeByScope(bindings);
  const secretScope = uniformScope(secretList);
  const bindingScope = uniformScope(bindingList);

  return (
    <aside className="side-panels" aria-label="Inspector state">
      <div className="inspector-header">
        <div>
          <h2>State</h2>
          <span>{loading ? "loading" : "read-only metadata"}</span>
        </div>
        <div className="inspector-actions">
          <button type="button" onClick={onRefresh}>
            refresh
          </button>
        </div>
      </div>

      {rollupChips.length > 0 ? (
        <section className="detail-panel conversation-stats-panel">
          <div className="section-header">
            <h2>Session</h2>
            <span>{rollupChips.length}</span>
          </div>
          <div className="conversation-rollup inspector-rollup">
            {rollupChips.map((chip) => (
              <span className="metric-chip" key={chip}>
                {chip}
              </span>
            ))}
          </div>
        </section>
      ) : null}

      <Panel
        title="Secrets"
        count={secretList.length}
        loading={loading}
        note={scopeNote(secretScope, "names + metadata only")}
      >
        {secretList.length === 0 && !loading ? (
          <div className="empty-inline">None.</div>
        ) : null}
        {loading && secretList.length === 0 ? <SkeletonPanelRows /> : null}
        {secretList.map(({ item, scope }) => (
          <SecretRow
            key={item.id}
            scope={scope}
            secret={item}
            showTag={secretScope === null}
          />
        ))}
      </Panel>

      <Panel
        title="Bindings"
        count={bindingList.length}
        loading={loading}
        note={scopeNote(bindingScope)}
      >
        {bindingList.length === 0 && !loading ? (
          <div className="empty-inline">None.</div>
        ) : null}
        {loading && bindingList.length === 0 ? <SkeletonPanelRows /> : null}
        {bindingList.map(({ item, scope }) => (
          <BindingRow
            key={item.id}
            record={item}
            scope={scope}
            showTag={bindingScope === null}
          />
        ))}
      </Panel>

      {memory !== null ? (
        <Panel
          title="Memory"
          count={memory.length}
          loading={loading}
          note="agent durable facts"
        >
          {memory.length === 0 ? (
            <div className="empty-inline">None.</div>
          ) : null}
          {memory.map((entry) => (
            <MemoryRow entry={entry} key={entry.id} />
          ))}
        </Panel>
      ) : null}

      <Panel title="Sandbox" count={sandboxes.length} loading={loading}>
        {loading && sandboxes.length === 0 ? <SkeletonPanelRows /> : null}
        <SandboxPanel sandboxes={sandboxes} />
      </Panel>
    </aside>
  );
}

function MemoryRow({ entry }: { entry: MemoryEntry }) {
  return (
    <div className="record-row memory-row">
      <p className="memory-text">{entry.text}</p>
      <div className="memory-meta">
        <time>{formatDateTime(entry.createdAt)}</time>
        <code>{shortId(entry.id)}</code>
      </div>
    </div>
  );
}

function scopeNote(scope: Scope | null, base?: string): string | undefined {
  const scopePart = scope ? `all ${scope}` : undefined;
  return [scopePart, base].filter(Boolean).join(" · ") || undefined;
}

function SandboxPanel({ sandboxes }: { sandboxes: SandboxView[] }) {
  return (
    <>
      {sandboxes.length === 0 ? (
        <div className="empty-inline">No sandbox records.</div>
      ) : null}
      {sandboxes.map((sandbox) => (
        <div className="sandbox-card" key={sandbox.id}>
          <div className="card-heading">
            <strong>{sandbox.name || sandbox.id}</strong>
            <span className={`state-pill state-${sandbox.state}`}>
              {sandbox.state}
            </span>
          </div>
          <div className="kv-grid compact">
            <span>id</span>
            <strong>{shortId(sandbox.id)}</strong>
            <span>provider</span>
            <strong>{sandbox.provider || "unknown"}</strong>
            <span>image</span>
            <strong>{sandbox.image || "unknown"}</strong>
            <span>workdir</span>
            <strong>{sandbox.defaultWorkdir || "unknown"}</strong>
            <span>network</span>
            <strong>
              {sandbox.enableNetworking == null
                ? "unknown"
                : String(sandbox.enableNetworking)}
            </strong>
          </div>
          {sandbox.mounts.length > 0 ? (
            <div className="tag-list">
              {sandbox.mounts.map((mount, index) => (
                <span key={`${mount.mount_path}-${index}`}>
                  mount {mount.mount_path} ({mount.mode})
                </span>
              ))}
            </div>
          ) : null}
          {sandbox.durableFileSystems.length > 0 ? (
            <div className="tag-list">
              {sandbox.durableFileSystems.map((fs) => (
                <span key={fs.name}>
                  durable {fs.name} → {fs.mount_path} ({fs.mode})
                </span>
              ))}
            </div>
          ) : null}
          {sandbox.snapshots.length > 0 ? (
            <div className="tag-list">
              {sandbox.snapshots.map((snapshot) => (
                <span key={snapshot}>snapshot {shortId(snapshot)}</span>
              ))}
            </div>
          ) : null}
          {sandbox.processes.length > 0 ? (
            <div className="process-list">
              {sandbox.processes.map((process) => (
                <details className="process-card" key={process.id}>
                  <summary>
                    <span className="process-name">
                      {process.name || shortId(process.id)}
                    </span>
                    <ProcessStatusBadge status={process.status} />
                  </summary>
                  <div className="kv-grid compact">
                    <span>command</span>
                    <strong className="process-command">
                      {process.command.join(" ") || "unknown"}
                    </strong>
                    <span>cwd</span>
                    <strong>{process.cwd || "default"}</strong>
                    <span>mode</span>
                    <strong>{process.mode || "unknown"}</strong>
                    <span>output</span>
                    <strong>
                      stdout {process.stdoutCount} / stderr{" "}
                      {process.stderrCount}
                    </strong>
                    <ProcessExitInfo status={process.status} />
                  </div>
                  {process.lastOutput ? (
                    <div className="process-output">
                      <div className="payload-label">last output</div>
                      <pre className="message-text">{process.lastOutput}</pre>
                    </div>
                  ) : null}
                  {process.providerState ? (
                    <JsonPreview
                      value={process.providerState}
                      label="provider state"
                    />
                  ) : null}
                </details>
              ))}
            </div>
          ) : null}
        </div>
      ))}
    </>
  );
}

function ProcessStatusBadge({
  status,
}: {
  status: SandboxProcessStatus | null;
}) {
  if (!status) {
    return <span className="proc-pill proc-unknown">unknown</span>;
  }
  return (
    <span className={`proc-pill proc-${processTone(status)}`}>
      {processBadgeLabel(status)}
    </span>
  );
}

function ProcessExitInfo({ status }: { status: SandboxProcessStatus | null }) {
  if (!status || status.type === "running") {
    return null;
  }
  if (status.type === "exited") {
    return (
      <>
        <span>exit code</span>
        <strong>{status.exit_code}</strong>
      </>
    );
  }
  if (status.type === "failed") {
    return (
      <>
        <span>error</span>
        <strong>{status.message}</strong>
      </>
    );
  }
  return (
    <>
      <span>state</span>
      <strong>cancelled</strong>
    </>
  );
}

function processTone(
  status: SandboxProcessStatus,
): "running" | "ok" | "error" | "idle" {
  switch (status.type) {
    case "running":
      return "running";
    case "exited":
      return status.exit_code === 0 ? "ok" : "error";
    case "failed":
      return "error";
    case "cancelled":
      return "idle";
  }
}

function processBadgeLabel(status: SandboxProcessStatus): string {
  switch (status.type) {
    case "running":
      return "running";
    case "exited":
      return `exit ${status.exit_code}`;
    case "failed":
      return "failed";
    case "cancelled":
      return "cancelled";
  }
}

function Panel({
  title,
  count,
  loading,
  note,
  defaultOpen = false,
  children,
}: {
  title: string;
  count: number;
  loading: boolean;
  note?: string;
  defaultOpen?: boolean;
  children: ReactNode;
}) {
  return (
    <details className="detail-panel" open={defaultOpen}>
      <summary className="section-header panel-summary">
        <span className="panel-summary-title">
          <svg
            aria-hidden="true"
            className="chevron"
            height="10"
            viewBox="0 0 10 10"
            width="10"
          >
            <path
              d="M3 1.5 6.5 5 3 8.5"
              fill="none"
              stroke="currentColor"
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeWidth="1.4"
            />
          </svg>
          <h2>{title}</h2>
        </span>
        <span>{loading ? "loading" : count}</span>
      </summary>
      {note ? <div className="panel-note">{note}</div> : null}
      {children}
    </details>
  );
}

function SecretRow({
  secret,
  scope,
  showTag,
}: {
  secret: SecretMetadata;
  scope: Scope;
  showTag: boolean;
}) {
  return (
    <div className="record-row">
      <div className="record-main">
        <strong>{secret.name}</strong>
        <span className="record-sub">
          {secret.type} · {formatDateTime(secret.created_at)}
        </span>
      </div>
      <div className="record-meta">
        {showTag ? <ScopeTag scope={scope} /> : null}
        <code>{shortId(secret.id)}</code>
      </div>
    </div>
  );
}

function BindingRow({
  record,
  scope,
  showTag,
}: {
  record: BindingRecord;
  scope: Scope;
  showTag: boolean;
}) {
  const binding = record.binding;
  const kind = binding.type;
  const summaryName = binding.type === "llm" ? binding.name : record.name;

  return (
    <details className="binding-row">
      <summary>
        <span className="binding-summary-name">
          <ChevronIcon />
          <strong>{summaryName}</strong>
        </span>
        <span className="record-meta">
          {showTag ? <ScopeTag scope={scope} /> : null}
          <span className="binding-kind">{kind}</span>
        </span>
      </summary>
      {binding.type === "llm" ? (
        <div className="kv-grid compact">
          <span>model</span>
          <strong>{binding.model}</strong>
          <span>base url</span>
          <strong>{binding.base_url || "default"}</strong>
          <span>secret</span>
          <strong>{shortId(binding.secret_id)}</strong>
        </div>
      ) : (
        <pre className="json-preview">{formatJson(binding)}</pre>
      )}
    </details>
  );
}

function ChevronIcon() {
  return (
    <svg
      className="chevron"
      width="10"
      height="10"
      viewBox="0 0 10 10"
      aria-hidden="true"
    >
      <path
        d="M3 1.5 6.5 5 3 8.5"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}

function SkeletonPanelRows() {
  return <SkeletonRows className="panel-skeleton" count={3} />;
}
