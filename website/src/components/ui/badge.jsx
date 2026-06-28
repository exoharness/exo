import { cva } from "class-variance-authority";
import { Slot } from "radix-ui";

import { cn } from "../../lib/utils.js";

const badgeVariants = cva(
  "group/badge inline-flex h-5 w-fit shrink-0 items-center justify-center gap-1 overflow-hidden rounded-2xl border border-transparent px-2 py-0.5 text-xs font-medium whitespace-nowrap transition-all focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/50 has-data-[icon=inline-end]:pr-1.5 has-data-[icon=inline-start]:pl-1.5 aria-invalid:border-destructive aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 [&>svg]:pointer-events-none [&>svg]:size-3!",
  {
    defaultVariants: {
      variant: "secondary",
    },
    variants: {
      variant: {
        danger:
          "border-destructive/30 bg-destructive/10 text-destructive dark:bg-destructive/20",
        default: "bg-primary text-primary-foreground [a]:hover:bg-primary/80",
        destructive:
          "bg-destructive/10 text-destructive focus-visible:ring-destructive/20 dark:bg-destructive/20 dark:focus-visible:ring-destructive/40 [a]:hover:bg-destructive/20",
        ghost:
          "hover:bg-muted hover:text-muted-foreground dark:hover:bg-muted/50",
        idle: "border-border bg-muted/50 text-muted-foreground",
        link: "text-primary underline-offset-4 hover:underline",
        neutral: "border-border bg-muted/50 text-muted-foreground",
        outline:
          "border-border text-foreground [a]:hover:bg-muted [a]:hover:text-muted-foreground",
        secondary:
          "bg-secondary text-secondary-foreground [a]:hover:bg-secondary/80",
        success:
          "border-emerald-500/30 bg-emerald-500/10 text-emerald-400 dark:bg-emerald-500/15",
        warning:
          "border-amber-500/30 bg-amber-500/10 text-amber-300 dark:bg-amber-500/15",
      },
    },
  },
);

export function Badge({
  asChild = false,
  className,
  variant = "secondary",
  ...props
}) {
  const Comp = asChild ? Slot.Root : "span";

  return (
    <Comp
      data-slot="badge"
      data-variant={variant}
      className={cn(badgeVariants({ variant, className }))}
      {...props}
    />
  );
}

export { badgeVariants };
