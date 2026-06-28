import { cn } from "../../lib/utils.js";

export function Message({ align = "start", className, ...props }) {
  return (
    <div
      data-slot="message"
      data-align={align}
      className={cn("ui-message", className)}
      {...props}
    />
  );
}

export function MessageGroup({ className, ...props }) {
  return (
    <div
      data-slot="message-group"
      className={cn("ui-message-group", className)}
      {...props}
    />
  );
}

export function MessageAvatar({ className, ...props }) {
  return (
    <div
      data-slot="message-avatar"
      className={cn("ui-message-avatar", className)}
      {...props}
    />
  );
}

export function MessageContent({ className, ...props }) {
  return (
    <div
      data-slot="message-content"
      className={cn("ui-message-content", className)}
      {...props}
    />
  );
}

export function MessageHeader({ className, ...props }) {
  return (
    <div
      data-slot="message-header"
      className={cn("ui-message-header", className)}
      {...props}
    />
  );
}

export function MessageFooter({ className, ...props }) {
  return (
    <div
      data-slot="message-footer"
      className={cn("ui-message-footer", className)}
      {...props}
    />
  );
}
