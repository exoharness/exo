import { cva } from "class-variance-authority";

import { cn } from "../../lib/utils.js";

const badgeVariants = cva("ui-badge", {
  defaultVariants: {
    variant: "secondary",
  },
  variants: {
    variant: {
      destructive: "",
      danger: "",
      idle: "",
      neutral: "",
      secondary: "",
      success: "",
      warning: "",
    },
  },
});

export function Badge({ className, variant = "secondary", ...props }) {
  return (
    <span
      data-slot="badge"
      data-variant={variant}
      className={cn(badgeVariants({ variant }), className)}
      {...props}
    />
  );
}
