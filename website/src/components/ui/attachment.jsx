import { cn } from "../../lib/utils.js";

export function Attachment({ className, ...props }) {
  return (
    <div
      data-slot="attachment"
      className={cn("ui-attachment", className)}
      {...props}
    />
  );
}

export function AttachmentContent({ className, ...props }) {
  return (
    <div
      data-slot="attachment-content"
      className={cn("ui-attachment-content", className)}
      {...props}
    />
  );
}

export function AttachmentMeta({ className, ...props }) {
  return (
    <div
      data-slot="attachment-meta"
      className={cn("ui-attachment-meta", className)}
      {...props}
    />
  );
}
