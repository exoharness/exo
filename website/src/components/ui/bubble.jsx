import { cn } from "../../lib/utils.js";

export function Bubble({
  align = "start",
  className,
  variant = "secondary",
  ...props
}) {
  return (
    <div
      data-slot="bubble"
      data-align={align}
      data-variant={variant}
      className={cn("ui-bubble", className)}
      {...props}
    />
  );
}

export function BubbleContent({ className, ...props }) {
  return (
    <div
      data-slot="bubble-content"
      className={cn("ui-bubble-content", className)}
      {...props}
    />
  );
}

export function BubbleReactions({ className, ...props }) {
  return (
    <div
      data-slot="bubble-reactions"
      className={cn("ui-bubble-reactions", className)}
      {...props}
    />
  );
}
