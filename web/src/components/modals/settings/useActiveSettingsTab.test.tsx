// @vitest-environment happy-dom
import { act } from "react";
import { createRoot } from "react-dom/client";
import { expect, it, vi } from "vite-plus/test";
import { useActiveSettingsTab } from "./useActiveSettingsTab";

it("reveals a mobile tab attached after the permissions response without scrolling the form", async () => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  const rect = vi
    .spyOn(HTMLElement.prototype, "getBoundingClientRect")
    .mockImplementation(function (this: HTMLElement) {
      return {
        left: this.dataset.tab ? 130 : 0,
        right: this.dataset.tab ? 170 : 100,
        width: this.dataset.tab ? 40 : 100,
        top: 0,
        bottom: 40,
        height: 40,
        x: 0,
        y: 0,
        toJSON: () => ({}),
      };
    });
  const width = vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(100);
  const observer = vi.fn();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe = observer;
      disconnect() {}
    },
  );
  function Probe({ ready }: { ready: boolean }) {
    const ref = useActiveSettingsTab("global", "webModels");
    return (
      <section style={{ overflowY: "auto" }}>
        <div data-strip="true">
          {ready ? (
            <button data-tab="true" ref={ref}>
              Web models
            </button>
          ) : null}
        </div>
      </section>
    );
  }
  const host = document.createElement("div");
  document.body.append(host);
  const root = createRoot(host);
  try {
    await act(async () => root.render(<Probe ready={false} />));
    await act(async () => root.render(<Probe ready={true} />));
    expect(host.querySelector("[data-strip]")!.scrollLeft).toBe(70);
    expect(host.querySelector("section")!.scrollTop).toBe(0);
    expect(observer).toHaveBeenCalledTimes(2);
  } finally {
    await act(async () => root.unmount());
    host.remove();
    rect.mockRestore();
    width.mockRestore();
    vi.unstubAllGlobals();
  }
});
