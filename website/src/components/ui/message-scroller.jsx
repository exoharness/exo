import * as React from "react";
import { MessageScroller as MessageScrollerPrimitive } from "@shadcn/react/message-scroller";
import { ArrowDownIcon } from "lucide-react";

import { cn } from "../../lib/utils.js";
import { Button } from "./button.jsx";

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
      className={cn(
        "group/message-scroller relative flex size-full min-h-0 flex-col overflow-hidden",
        className,
      )}
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
        className={cn(
          "size-full min-h-0 min-w-0 scroll-fade-b scrollbar-thin scrollbar-gutter-stable overflow-y-auto overscroll-contain contain-content data-autoscrolling:scrollbar-none",
          className,
        )}
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
        className={cn("flex h-max min-h-full flex-col gap-8", className)}
        spacerClassName={spacerClassName}
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
        className={cn(
          "min-w-0 shrink-0 [contain-intrinsic-size:auto_10rem] [content-visibility:auto]",
          className,
        )}
        {...props}
      />
    );
  },
);

export const MessageScrollerButton = React.forwardRef(
  function MessageScrollerButton(
    {
      children,
      className,
      direction = "end",
      render,
      size = "icon-sm",
      variant = "secondary",
      ...props
    },
    ref,
  ) {
    return (
      <MessageScrollerPrimitive.Button
        ref={ref}
        data-slot="message-scroller-button"
        data-direction={direction}
        data-size={size}
        data-variant={variant}
        direction={direction}
        className={cn(
          "absolute inset-s-1/2 -translate-x-1/2 border-border bg-background text-foreground transition-[translate,scale,opacity] duration-200 hover:bg-muted hover:text-foreground data-[active=false]:pointer-events-none data-[active=false]:scale-95 data-[active=false]:opacity-0 data-[active=false]:duration-400 data-[active=false]:ease-[cubic-bezier(0.7,0,0.84,0)] data-[active=true]:translate-y-0 data-[active=true]:scale-100 data-[active=true]:opacity-100 data-[active=true]:ease-[cubic-bezier(0.23,1,0.32,1)] data-[direction=end]:bottom-4 data-[direction=end]:data-[active=false]:translate-y-full data-[direction=start]:top-4 data-[direction=start]:data-[active=false]:-translate-y-full rtl:translate-x-1/2 data-[direction=start]:[&_svg]:rotate-180",
          className,
        )}
        render={render ?? <Button variant={variant} size={size} />}
        {...props}
      >
        {children ?? (
          <>
            <ArrowDownIcon />
            <span className="sr-only">
              {direction === "end" ? "Scroll to end" : "Scroll to start"}
            </span>
          </>
        )}
      </MessageScrollerPrimitive.Button>
    );
  },
);

export {
  useMessageScroller,
  useMessageScrollerScrollable,
  useMessageScrollerVisibility,
} from "@shadcn/react/message-scroller";
