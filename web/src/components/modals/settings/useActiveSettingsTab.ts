import { useLayoutEffect, useState } from "react";

/** Reveal the active mobile section without scrolling the settings form or page. */
export function useActiveSettingsTab(scope: string, activeTab: string) {
  const [tab, setTab] = useState<HTMLButtonElement | null>(null);

  useLayoutEffect(() => {
    const strip = tab?.parentElement;
    if (!tab || !strip) return;
    const reveal = () => {
      if (strip.clientWidth === 0) return;
      const item = tab.getBoundingClientRect();
      const viewport = strip.getBoundingClientRect();
      if (item.left < viewport.left) strip.scrollLeft -= viewport.left - item.left;
      else if (item.right > viewport.right) strip.scrollLeft += item.right - viewport.right;
    };
    reveal();
    const observer = new ResizeObserver(reveal);
    observer.observe(strip);
    observer.observe(tab);
    return () => observer.disconnect();
  }, [scope, activeTab, tab]);

  // Global sections can appear after permissions load; react to DOM attachment too.
  return setTab;
}
