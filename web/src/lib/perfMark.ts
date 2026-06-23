const startMark = (label: string) => `${label}:start`;
const endMark = (label: string) => `${label}:end`;

export function perfStart(label: string): void {
  performance.mark(startMark(label));
}

export function perfEnd(label: string): number | null {
  performance.mark(endMark(label));
  try {
    performance.measure(label, startMark(label), endMark(label));
  } catch {
    return null;
  }

  const entries = performance.getEntriesByName(label, "measure");
  const latest = entries[entries.length - 1];
  return latest ? latest.duration : null;
}

export function perfMeasure(label: string): number | null {
  return perfEnd(label);
}

export function perfLog(
  label: string,
  durationMs?: number | null,
): number | null {
  const duration = durationMs ?? perfEnd(label);
  if (duration != null) {
    console.log(`[perf] ${label}: ${duration.toFixed(2)}ms`);
  }
  return duration;
}

export function perfClear(label?: string): void {
  if (label) {
    performance.clearMarks(startMark(label));
    performance.clearMarks(endMark(label));
    performance.clearMeasures(label);
    return;
  }

  performance.clearMarks();
  performance.clearMeasures();
}
