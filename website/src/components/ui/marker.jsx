import { cn } from "../../lib/utils.js";

export function Marker({ className, variant = "default", ...props }) {
  return (
    <div
      data-slot="marker"
      data-variant={variant}
      className={cn("ui-marker", className)}
      {...props}
    />
  );
}

export function MarkerIcon({ className, ...props }) {
  return (
    <span
      data-slot="marker-icon"
      className={cn("ui-marker-icon", className)}
      {...props}
    />
  );
}

export function MarkerContent({ className, ...props }) {
  return (
    <span
      data-slot="marker-content"
      className={cn("ui-marker-content", className)}
      {...props}
    />
  );
}
