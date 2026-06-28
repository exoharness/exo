import { cn } from "../../lib/utils.js";

export function Avatar({ className, ...props }) {
  return (
    <div data-slot="avatar" className={cn("ui-avatar", className)} {...props} />
  );
}

export function AvatarFallback({ className, ...props }) {
  return (
    <span
      data-slot="avatar-fallback"
      className={cn("ui-avatar-fallback", className)}
      {...props}
    />
  );
}
