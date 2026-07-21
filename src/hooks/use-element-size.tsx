"use client";

import * as React from "react";

/**
 * Observe an element's content-box size.
 * `isReady` is true only when both width and height are > 0 so chart libs
 * (Recharts ResponsiveContainer) never mount with a zero/unknown box.
 *
 * `ref` is a callback ref — works with `ref={chartRef}` and composed refs.
 */
export function useElementSize<T extends HTMLElement>() {
  const [element, setElement] = React.useState<T | null>(null);
  const [size, setSize] = React.useState({ width: 0, height: 0 });

  const setRef = React.useCallback((el: T | null) => {
    setElement((prev) => (prev === el ? prev : el));
  }, []);

  React.useLayoutEffect(() => {
    if (!element) {
      setSize({ width: 0, height: 0 });
      return;
    }

    let raf = 0;
    const updateSize = () => {
      const { width, height } = element.getBoundingClientRect();
      setSize((prev) => {
        const next = {
          width: Math.max(0, Math.round(width)),
          height: Math.max(0, Math.round(height)),
        };
        if (prev.width === next.width && prev.height === next.height) {
          return prev;
        }
        return next;
      });
    };

    updateSize();
    // Flex/table cells often report 0 on the first commit; remeasure next frame.
    raf = window.requestAnimationFrame(updateSize);

    const observer = new ResizeObserver(() => {
      updateSize();
    });
    observer.observe(element);

    return () => {
      window.cancelAnimationFrame(raf);
      observer.disconnect();
    };
  }, [element]);

  return {
    /** Callback ref — use as `ref={chartRef}`. */
    ref: setRef,
    setRef,
    width: size.width,
    height: size.height,
    isReady: size.width > 0 && size.height > 0,
  };
}
