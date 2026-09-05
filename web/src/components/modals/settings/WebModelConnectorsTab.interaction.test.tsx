// @vitest-environment happy-dom
import { act, useEffect, type ComponentProps } from "react";
import { createRoot, type Root } from "react-dom/client";
import { createInstance } from "i18next";
import { I18nextProvider } from "react-i18next";
import { afterEach, beforeEach, describe, expect, it, vi } from "vite-plus/test";
import zh from "../../../i18n/locales/zh/settings.json";
import WebModelConnectorsTab from "./WebModelConnectorsTab";

const mocks = vi.hoisted(() => ({
  fetchGroups: vi.fn(),
  fetchActors: vi.fn(),
  fetchWebModelConnectors: vi.fn(),
  fetchRemoteAccessState: vi.fn(),
  fetchWebModelBrowserSession: vi.fn(),
  fetchWebModelBrowserSurfaceSession: vi.fn(),
  createWebModelConnector: vi.fn(),
  createWebModelConnectorBinding: vi.fn(),
  revokeWebModelConnector: vi.fn(),
  openWebModelBrowserSession: vi.fn(),
  closeWebModelBrowserSession: vi.fn(),
  bindCurrentWebModelBrowserConversation: vi.fn(),
  copy: vi.fn(),
  viewerStart: vi.fn(),
}));
vi.mock("../../../services/api", () => ({
  ...mocks,
  getWebModelBrowserSurfaceWebSocketUrl: () => "ws://localhost/test-only",
}));
vi.mock("../../../utils/copy", () => ({ copyTextToClipboard: mocks.copy }));
vi.mock("../../browser/ProjectedBrowserSurfacePanel", () => ({
  ProjectedBrowserSurfacePanel: ({
    loadSession,
    startSession,
  }: {
    loadSession: () => Promise<unknown>;
    startSession?: unknown;
  }) => {
    useEffect(() => {
      mocks.viewerStart(startSession);
      void loadSession();
    }, [loadSession, startSession]);
    return <div>Test browser preview</div>;
  },
}));
const ok = <T,>(result: T) => ({ ok: true as const, result });
const groupA = { group_id: "g_a", title: "网页组", state: "stopped" };
const groupB = { group_id: "g_b", title: "本地组", state: "stopped" };
const groupC = { group_id: "g_c", title: "空工作组", state: "stopped" };
const leadA = {
  id: "web-lead",
  title: "网页组长甲",
  role: "foreman",
  runtime: "web_model",
  running: false,
};
const leadB = {
  id: "local-lead",
  title: "本地组长乙",
  role: "foreman",
  runtime: "opencode",
  running: false,
};
const storedSession = {
  active: true,
  ready: true,
  login_required: false,
  visibility: "visible",
  tab_url: "https://chatgpt.com/c/other-group",
  conversation_url: "https://chatgpt.com/c/saved-a",
  delivery_target: {
    kind: "existing_chat",
    state: "bound_existing_chat",
    url: "https://chatgpt.com/c/saved-a",
  },
};
let host: HTMLDivElement;
let root: Root;
let callbacks: Pick<
  ComponentProps<typeof WebModelConnectorsTab>,
  "onCreateGroup" | "onCreateActor" | "onEditActor" | "onOpenGuidance"
>;

const button = (id: string) => {
  const el = document.querySelector<HTMLElement>(`[data-testid="${id}"]`);
  expect(el, `missing ${id}`).not.toBeNull();
  return el!;
};
const settle = async () => {
  await act(async () => {
    await new Promise((r) => setTimeout(r, 0));
  });
};
const click = async (id: string) => {
  await act(async () => button(id).click());
  await settle();
};
const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
};
async function render() {
  const i18n = createInstance();
  await i18n.init({
    lng: "zh",
    fallbackLng: "zh",
    resources: { zh: { settings: zh } },
    interpolation: { escapeValue: false },
  });
  await act(async () =>
    root.render(
      <I18nextProvider i18n={i18n}>
        <WebModelConnectorsTab isDark={false} currentGroupId="g_a" {...callbacks} />
      </I18nextProvider>,
    ),
  );
  await settle();
}
beforeEach(() => {
  vi.clearAllMocks();
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  callbacks = {
    onCreateGroup: vi.fn(),
    onCreateActor: vi.fn(),
    onEditActor: vi.fn(),
    onOpenGuidance: vi.fn(),
  };
  mocks.fetchGroups.mockResolvedValue(ok({ groups: [groupA, groupB, groupC] }));
  mocks.fetchActors.mockImplementation(async (gid: string) =>
    ok({ actors: gid === "g_a" ? [leadA] : gid === "g_b" ? [leadB] : [] }),
  );
  mocks.fetchWebModelConnectors.mockResolvedValue(
    ok({
      connectors: [
        { connector_id: "conn-a", group_id: "g_a", actor_id: "web-lead", session_bound: true },
      ],
    }),
  );
  mocks.fetchRemoteAccessState.mockResolvedValue(ok({ remote_access: { config: {} } }));
  mocks.fetchWebModelBrowserSession.mockResolvedValue(ok({ browser_session: storedSession }));
  mocks.fetchWebModelBrowserSurfaceSession.mockResolvedValue(
    ok({ browser_surface: { state: "ready" } }),
  );
  mocks.createWebModelConnectorBinding.mockResolvedValue(
    ok({
      code: "test-code",
      group_id: "g_a",
      actor_id: "web-lead",
      binding_expires_at: new Date(Date.now() + 600_000).toISOString(),
    }),
  );
  mocks.bindCurrentWebModelBrowserConversation.mockResolvedValue(
    ok({ browser_session: storedSession }),
  );
  mocks.copy.mockResolvedValue(true);
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.restoreAllMocks();
});

describe("restored connection page", () => {
  it("lists local and empty groups, routes edits to the selected group, and keeps preview display read-only", async () => {
    await render();
    expect((button("web-model-original-reference") as HTMLDetailsElement).open).toBe(false);
    await click("web-model-view-surface");
    expect(mocks.viewerStart).toHaveBeenCalledWith(undefined);
    await click("web-model-hide-surface");
    expect(mocks.openWebModelBrowserSession).not.toHaveBeenCalled();
    expect(mocks.closeWebModelBrowserSession).not.toHaveBeenCalled();
    await click("web-model-group-g_b");
    expect(button("web-model-selected-group").textContent).toContain("本地组长乙");
    await click("web-model-change-foreman");
    await click("web-model-change-foreman-local");
    expect(callbacks.onEditActor).toHaveBeenCalledWith("g_b", "local-lead", "local");
    await click("web-model-add-member");
    expect(callbacks.onCreateActor).toHaveBeenCalledWith("g_b", { role: "peer" });
    await click("web-model-group-g_c");
    await click("web-model-add-foreman");
    expect(callbacks.onCreateActor).toHaveBeenCalledWith("g_c", { role: "foreman" });
    await click("web-model-new-group");
    expect(callbacks.onCreateGroup).toHaveBeenCalledWith("g_c");
    expect(mocks.createWebModelConnector).not.toHaveBeenCalled();
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
  });

  it("does not infer another group's current URL and never saves a target on selection or cancellation", async () => {
    mocks.fetchWebModelBrowserSession.mockResolvedValue(
      ok({ browser_session: { active: true, tab_url: "https://chatgpt.com/c/other-group" } }),
    );
    await render();
    await click("web-model-change-foreman");
    await click("web-model-change-foreman-web");
    expect((button("web-model-target-url") as HTMLInputElement).value).toBe("");
    await click("web-model-editor-current");
    mocks.fetchWebModelBrowserSession.mockResolvedValue(
      ok({ browser_session: { active: true, tab_url: "https://chatgpt.com/c/freshly-read" } }),
    );
    await click("web-model-read-current-chat");
    await click("web-model-editor-pasted");
    expect((button("web-model-target-url") as HTMLInputElement).value).toBe(
      "https://chatgpt.com/c/freshly-read",
    );
    await click("web-model-editor-new");
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    await click("web-model-cancel-target-edit");
    expect((button("web-model-target-url") as HTMLInputElement).value).toBe("");
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
  });

  it("rejects a late actor list from the previously selected group and preserves selection on refresh", async () => {
    const delayed = deferred<ReturnType<typeof ok<{ actors: (typeof leadA)[] }>>>();
    mocks.fetchActors.mockImplementation((gid: string) =>
      gid === "g_a" ? delayed.promise : Promise.resolve(ok({ actors: [leadB] })),
    );
    await render();
    await click("web-model-group-g_b");
    await act(async () =>
      delayed.resolve(ok({ actors: [{ ...leadA, title: "LATE_WRONG_GROUP" }] })),
    );
    await settle();
    expect(button("web-model-selected-group").textContent).toContain("本地组长乙");
    expect(button("web-model-selected-group").textContent).not.toContain("LATE_WRONG_GROUP");
    const refresh = Array.from(host.querySelectorAll("button")).find(
      (el) => el.textContent === "刷新",
    )!;
    await act(async () => refresh.click());
    await settle();
    expect(button("web-model-group-g_b").getAttribute("aria-pressed")).toBe("true");
  });

  it("does not copy a late connection code into the newly selected group", async () => {
    const delayed =
      deferred<
        ReturnType<
          typeof ok<{
            code: string;
            group_id: string;
            actor_id: string;
            binding_expires_at: string;
          }>
        >
      >();
    mocks.createWebModelConnectorBinding.mockReturnValue(delayed.promise);
    await render();
    await click("web-model-copy-instructions");
    await click("web-model-group-g_b");
    await act(async () =>
      delayed.resolve(
        ok({
          code: "old-selection",
          group_id: "g_a",
          actor_id: "web-lead",
          binding_expires_at: new Date(Date.now() + 600_000).toISOString(),
        }),
      ),
    );
    await settle();
    expect(mocks.copy).not.toHaveBeenCalled();
    expect(button("web-model-selected-group").textContent).toContain("本地组长乙");
  });

  it("copies a fresh escaped code for the selected group without claiming return delivery", async () => {
    mocks.createWebModelConnectorBinding.mockResolvedValue(
      ok({
        code: 'test"code',
        group_id: "g_a",
        actor_id: "web-lead",
        binding_expires_at: new Date(Date.now() + 600_000).toISOString(),
      }),
    );
    await render();
    await click("web-model-copy-instructions");
    const copied = mocks.copy.mock.calls[0]?.[0] as string;
    expect(copied).toContain('"test\\"code"');
    expect(copied).toContain("网页组");
    expect(copied).toContain("web-lead");
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    expect(button("web-model-identity-status").textContent).toContain("不代表报告已经成功回传");
  });

  it("refreshes external binding changes without starting a browser or replacing the selected group", async () => {
    let poll: (() => void) | undefined;
    const originalInterval = window.setInterval.bind(window);
    vi.spyOn(window, "setInterval").mockImplementation((handler) => {
      poll = () => handler(undefined);
      // happy-dom returns a numeric browser timer; the spy also sees Node's ambient overload.
      return originalInterval(() => {}, 60_000) as unknown as ReturnType<typeof setInterval>;
    });
    vi.spyOn(document, "hidden", "get").mockReturnValue(false);
    mocks.fetchWebModelConnectors.mockResolvedValue(ok({ connectors: [] }));
    await render();
    expect(button("web-model-identity-status").textContent).toContain("身份未绑定");
    mocks.fetchWebModelConnectors.mockResolvedValue(
      ok({
        connectors: [
          { connector_id: "conn-a", group_id: "g_a", actor_id: "web-lead", session_bound: true },
        ],
      }),
    );
    expect(poll).toBeDefined();
    await act(async () => poll?.());
    await settle();
    expect(button("web-model-identity-status").textContent).toContain("身份已绑定");
    expect(mocks.fetchWebModelBrowserSession).toHaveBeenLastCalledWith("g_a", "web-lead", {
      inspect: false,
    });
    expect(mocks.openWebModelBrowserSession).not.toHaveBeenCalled();
  });

  it("disconnects only the chosen connector without closing the shared browser", async () => {
    mocks.revokeWebModelConnector.mockImplementation(async () => {
      mocks.fetchWebModelConnectors.mockResolvedValue(ok({ connectors: [] }));
      return ok({ revoked: true, connector_id: "conn-a" });
    });
    await render();
    await click("web-model-disconnect");
    expect(mocks.revokeWebModelConnector).toHaveBeenCalledExactlyOnceWith("conn-a");
    expect(button("web-model-identity-status").textContent).toContain("身份未绑定");
    expect(mocks.closeWebModelBrowserSession).not.toHaveBeenCalled();
  });

  it("saves a new-chat choice only after explicit confirmation using the current native API", async () => {
    await render();
    await click("web-model-change-foreman");
    await click("web-model-change-foreman-web");
    await click("web-model-editor-new");
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    await click("web-model-save-target");
    expect(mocks.bindCurrentWebModelBrowserConversation).toHaveBeenCalledExactlyOnceWith({
      groupId: "g_a",
      actorId: "web-lead",
      conversationUrl: "https://chatgpt.com/",
      newChat: true,
    });
    expect(mocks.createWebModelConnectorBinding).not.toHaveBeenCalled();
  });

  it("shows a closed browser as closed without mounting a starting preview", async () => {
    mocks.fetchWebModelBrowserSession.mockResolvedValue(
      ok({ browser_session: { active: false, ready: false } }),
    );
    await render();
    await click("web-model-view-surface");
    expect(button("web-model-browser-surface").textContent).toContain("浏览器尚未打开");
    expect(mocks.viewerStart).not.toHaveBeenCalled();
    expect(mocks.openWebModelBrowserSession).not.toHaveBeenCalled();
  });
});
