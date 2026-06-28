import * as React from "react";

import { cn } from "../../lib/utils.js";

export const Textarea = React.forwardRef(function Textarea(
  { className, ...props },
  ref,
) {
  return (
    <textarea
      ref={ref}
      data-slot="textarea"
      className={cn("ui-textarea", className)}
      {...props}
    />
  );
});
