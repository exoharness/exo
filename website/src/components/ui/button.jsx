import * as React from "react";
import { cva } from "class-variance-authority";

import { cn } from "../../lib/utils.js";

const buttonVariants = cva("ui-button", {
  defaultVariants: {
    size: "default",
    variant: "default",
  },
  variants: {
    size: {
      default: "",
      icon: "",
      "icon-xs": "",
    },
    variant: {
      default: "",
      ghost: "",
    },
  },
});

export const Button = React.forwardRef(function Button(
  { className, size = "default", variant = "default", ...props },
  ref,
) {
  return (
    <button
      ref={ref}
      data-slot="button"
      data-size={size}
      data-variant={variant}
      className={cn(buttonVariants({ size, variant }), className)}
      {...props}
    />
  );
});
