import * as React from "react";
import { MessageScroller as MessageScrollerPrimitive } from "@shadcn/react/message-scroller";

import { cn } from "../../lib/utils.js";

export function MessageScrollerProvider({
  autoScroll = true,
  defaultScrollPosition = "end",
  ...props
}) {
  return (
    <MessageScrollerPrimitive.Provider
      autoScroll={autoScroll}
      defaultScrollPosition={defaultScrollPosition}
      {...props}
    />
  );
}

export const MessageScroller = React.forwardRef(function MessageScroller(
  { className, ...props },
  ref,
) {
  return (
    <MessageScrollerPrimitive.Root
      ref={ref}
      data-slot="message-scroller"
      className={cn("ui-message-scroller", className)}
      {...props}
    />
  );
});

export const MessageScrollerViewport = React.forwardRef(
  function MessageScrollerViewport({ className, ...props }, ref) {
    return (
      <MessageScrollerPrimitive.Viewport
        ref={ref}
        data-slot="message-scroller-viewport"
        className={cn("ui-message-scroller-viewport", className)}
        {...props}
      />
    );
  },
);

export const MessageScrollerContent = React.forwardRef(
  function MessageScrollerContent(
    { className, spacerClassName, ...props },
    ref,
  ) {
    return (
      <MessageScrollerPrimitive.Content
        ref={ref}
        data-slot="message-scroller-content"
        className={cn("ui-message-scroller-content", className)}
        spacerClassName={cn("ui-message-scroller-spacer", spacerClassName)}
        {...props}
      />
    );
  },
);

export const MessageScrollerItem = React.forwardRef(
  function MessageScrollerItem({ className, ...props }, ref) {
    return (
      <MessageScrollerPrimitive.Item
        ref={ref}
        data-slot="message-scroller-item"
        className={cn("ui-message-scroller-item", className)}
        {...props}
      />
    );
  },
);

export const MessageScrollerButton = React.forwardRef(
  function MessageScrollerButton({ className, ...props }, ref) {
    return (
      <MessageScrollerPrimitive.Button
        ref={ref}
        data-slot="message-scroller-button"
        className={cn("ui-message-scroller-button", className)}
        {...props}
      />
    );
  },
);

export {
  useMessageScroller,
  useMessageScrollerScrollable,
  useMessageScrollerVisibility,
} from "@shadcn/react/message-scroller";
