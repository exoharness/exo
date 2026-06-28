import * as React from "react";

import { cn } from "../../lib/utils.js";

export const MessageScroller = React.forwardRef(function MessageScroller(
  { className, ...props },
  ref,
) {
  return (
    <div
      ref={ref}
      data-slot="message-scroller"
      className={cn("ui-message-scroller", className)}
      {...props}
    />
  );
});
