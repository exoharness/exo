export type HealthState =
  | { status: "idle"; message: string }
  | { status: "checking"; message: string }
  | { status: "ok"; message: string }
  | { status: "error"; message: string };

interface HealthBadgeProps {
  health: HealthState;
}

export function HealthBadge({ health }: HealthBadgeProps) {
  return (
    <span
      aria-label={`health: ${health.status}`}
      className={`health-badge health-${health.status}`}
      role="status"
      title={health.message}
    >
      <span className="health-dot" />
    </span>
  );
}
