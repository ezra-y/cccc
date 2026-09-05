// @vitest-environment happy-dom
import { act, useEffect, type ReactNode } from "react";
import { createRoot, type Root } from "react-dom/client";
import { createInstance } from "i18next";
import { I18nextProvider } from "react-i18next";
import { beforeEach, afterEach, describe, expect, it, vi } from "vite-plus/test";
import settings from "../i18n/locales/zh/settings.json";
import actors from "../i18n/locales/zh/actors.json";
import common from "../i18n/locales/zh/common.json";
import WebModelConnectorsTab from "./modals/settings/WebModelConnectorsTab";
import { GroupMembersMenu } from "./layout/GroupMembersMenu";
import type { Actor } from "../types";

const mocks = vi.hoisted(() => ({
  fetchGroups: vi.fn(),
  fetchActors: vi.fn(),
  fetchWebModelConnectors: vi.fn(),
  fetchRemoteAccessState: vi.fn(),
  fetchWebModelBrowserSession: vi.fn(),
  fetchWebModelBrowserSurfaceSession: vi.fn(),
  openWebModelBrowserSurfaceSession: vi.fn(),
  closeWebModelBrowserSurfaceSession: vi.fn(),
  createWebModelConnector: vi.fn(),
  createWebModelConnectorBinding: vi.fn(),
  revokeWebModelConnector: vi.fn(),
  bindCurrentWebModelBrowserConversation: vi.fn(),
  fetchRuntimes: vi.fn(),
  copy: vi.fn(),
  previewStart: vi.fn(),
  openModal: vi.fn(),
  setRole: vi.fn(),
  setRuntime: vi.fn(),
  setCommand: vi.fn(),
  group: { selectedGroupId: "g_a", actors: [] as unknown[], setRuntimes: vi.fn() },
  showError: vi.fn(),
}));
vi.mock("../services/api", () => ({
  ...mocks,
  getWebModelBrowserSurfaceWebSocketUrl: () => "ws://local.invalid/preview",
}));
vi.mock("../utils/copy", () => ({ copyTextToClipboard: mocks.copy }));
vi.mock("../stores", () => ({
  useFormStore: {
    getState: () => ({
      setNewActorRole: mocks.setRole,
      setEditActorRuntime: mocks.setRuntime,
      setEditActorCommand: mocks.setCommand,
    }),
  },
  useGroupStore: { getState: () => mocks.group },
  useModalStore: { getState: () => ({ openModal: mocks.openModal }) },
  useUIStore: { getState: () => ({ showError: mocks.showError }) },
}));
vi.mock("./browser/ProjectedBrowserSurfacePanel", () => ({
  ProjectedBrowserSurfacePanel: ({
    loadSession,
    startSession,
  }: {
    loadSession: () => Promise<unknown>;
    startSession?: unknown;
  }) => {
    useEffect(() => {
      mocks.previewStart(startSession);
      void loadSession();
    }, [loadSession, startSession]);
    return <div data-testid="native-preview">Preview</div>;
  },
}));
const ok = <T,>(result: T) => ({ ok: true as const, result });
const lead = {
  id: "lead",
  title: "组长甲",
  runtime: "web_model",
  role: "foreman",
  enabled: true,
  running: false,
} as Actor;
const other = { ...lead, title: "组长乙" };
const session = (gid: string) => ({
  active: true,
  ready: true,
  login_required: false,
  tab_url: "https://chatgpt.com/c/other-live-chat",
  conversation_url: `https://chatgpt.com/c/${gid}`,
  delivery_target: {
    kind: "existing_chat",
    state: "bound_existing_chat",
    url: `https://chatgpt.com/c/${gid}`,
  },
});
let host: HTMLDivElement;
let root: Root;
const wait = () =>
  act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  });
const find = (selector: string) => {
  const el = document.querySelector<HTMLElement>(selector);
  expect(el, selector).not.toBeNull();
  return el!;
};
const click = async (selector: string) => {
  await act(async () => find(selector).click());
  await wait();
};
const clickText = async (text: string) => {
  const el = [...host.querySelectorAll("button")].find((item) =>
    item.textContent?.startsWith(text),
  );
  expect(el, text).toBeTruthy();
  await act(async () => el!.click());
  await wait();
};
const choose = async (gid: string) => {
  await act(async () => {
    const el = find("#t05-web-group") as HTMLSelectElement;
    el.value = gid;
    el.dispatchEvent(new Event("change", { bubbles: true }));
  });
  await wait();
};
const deferred = <T,>() => {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { resolve, promise };
};
async function render(
  content: ReactNode = <WebModelConnectorsTab isDark={false} currentGroupId="g_a" />,
) {
  const i18n = createInstance();
  await i18n.init({
    lng: "zh",
    resources: { zh: { settings, actors, common } },
    interpolation: { escapeValue: false },
  });
  await act(async () => root.render(<I18nextProvider i18n={i18n}>{content}</I18nextProvider>));
  await wait();
}
beforeEach(() => {
  vi.clearAllMocks();
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  host = document.createElement("div");
  document.body.append(host);
  root = createRoot(host);
  mocks.group.selectedGroupId = "g_a";
  mocks.group.actors = [lead];
  mocks.fetchGroups.mockResolvedValue(
    ok({
      groups: [
        { group_id: "g_a", title: "甲组" },
        { group_id: "g_b", title: "乙组" },
        { group_id: "g_empty", title: "空组" },
      ],
    }),
  );
  mocks.fetchActors.mockImplementation(async (gid: string) =>
    ok({ actors: gid === "g_empty" ? [] : [gid === "g_a" ? lead : other] }),
  );
  mocks.fetchWebModelConnectors.mockResolvedValue(
    ok({
      connectors: ["g_a", "g_b"].map((gid) => ({
        connector_id: `conn-${gid}`,
        group_id: gid,
        actor_id: "lead",
        session_bound: true,
      })),
    }),
  );
  mocks.fetchRemoteAccessState.mockResolvedValue(ok({ remote_access: { config: {} } }));
  mocks.fetchWebModelBrowserSession.mockImplementation(async (gid: string) =>
    ok({ browser_session: session(gid) }),
  );
  mocks.fetchWebModelBrowserSurfaceSession.mockImplementation(async (gid: string) =>
    ok({ browser_session: session(gid), browser_surface: { active: true, state: "ready" } }),
  );
  mocks.createWebModelConnectorBinding.mockImplementation(async (id: string) =>
    ok({
      code: 'one"code',
      group_id: id.slice(5),
      actor_id: "lead",
      binding_expires_at: new Date(Date.now() + 600_000).toISOString(),
    }),
  );
  mocks.copy.mockResolvedValue(true);
  mocks.fetchRuntimes.mockResolvedValue(
    ok({ runtimes: [{ name: "opencode", available: true, recommended_command: "opencode" }] }),
  );
});
afterEach(async () => {
  await act(async () => root.unmount());
  host.remove();
  vi.restoreAllMocks();
});

describe("minimal overlay on upstream UI", () => {
  it("keeps native setup sections and only adds a group-scoped connection selector", async () => {
    await render();
    expect(host.textContent).toContain("ChatGPT Web Model");
    expect(host.textContent).not.toContain("工作组概览");
    expect(host.textContent).not.toContain("添加成员");
    expect((find('[data-t05-change="legacy-setup"]') as HTMLDetailsElement).open).toBe(false);
    await choose("g_b");
    expect((find('input[placeholder="https://chatgpt.com/c/..."]') as HTMLInputElement).value).toBe(
      "https://chatgpt.com/c/g_b",
    );
    await clickText("刷新");
    expect((find("#t05-web-group") as HTMLSelectElement).value).toBe("g_b");
    expect(mocks.openWebModelBrowserSurfaceSession).not.toHaveBeenCalled();
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    await choose("g_empty");
    expect(host.textContent).toContain("本组没有网页成员");
    expect(host.textContent).not.toContain("chatgpt.com/c/g_b");
  });
  it("copies only the chosen group's fresh code and marks every added control", async () => {
    await render();
    await choose("g_b");
    await click('[data-t05-change="copy-binding"]');
    expect(mocks.createWebModelConnectorBinding).toHaveBeenLastCalledWith("conn-g_b");
    expect(mocks.copy.mock.calls[0][0]).toContain("乙组");
    expect(mocks.copy.mock.calls[0][0]).toContain('"one\\"code"');
    for (const region of host.querySelectorAll("[data-t05-change]"))
      expect(
        region.querySelector("[data-t05-mark] circle"),
        region.getAttribute("data-t05-change")!,
      ).not.toBeNull();
  });
  it("discards a late actor response and a late code after switching groups", async () => {
    const delayedActors = deferred<ReturnType<typeof ok<{ actors: Actor[] }>>>();
    mocks.fetchActors.mockImplementation((gid: string) =>
      gid === "g_a" ? delayedActors.promise : Promise.resolve(ok({ actors: [other] })),
    );
    await render();
    await choose("g_b");
    await act(async () => delayedActors.resolve(ok({ actors: [lead] })));
    await wait();
    expect(host.textContent).not.toContain("组长甲 的 ChatGPT");
    const delayedCode =
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
    mocks.createWebModelConnectorBinding.mockReturnValue(delayedCode.promise);
    await click('[data-t05-change="copy-binding"]');
    await choose("g_a");
    await act(async () =>
      delayedCode.resolve(
        ok({
          code: "late",
          group_id: "g_b",
          actor_id: "lead",
          binding_expires_at: new Date(Date.now() + 10000).toISOString(),
        }),
      ),
    );
    await wait();
    expect(mocks.copy).not.toHaveBeenCalled();
  });
  it("showing and hiding the native projection does not start or close the browser", async () => {
    await render();
    await click('[data-t05-change="preview-toggle"]');
    expect(mocks.previewStart).toHaveBeenCalledWith(undefined);
    await click('[data-t05-change="preview-toggle"]');
    expect(document.querySelector('[data-testid="native-preview"]')).toBeNull();
    expect(mocks.openWebModelBrowserSurfaceSession).not.toHaveBeenCalled();
    expect(mocks.closeWebModelBrowserSurfaceSession).not.toHaveBeenCalled();
  });
  it("refuses expired binding codes without copying or changing the return target", async () => {
    mocks.createWebModelConnectorBinding.mockResolvedValue(
      ok({
        code: "expired",
        group_id: "g_a",
        actor_id: "lead",
        binding_expires_at: new Date(0).toISOString(),
      }),
    );
    await render();
    await click('[data-t05-change="copy-binding"]');
    expect(mocks.copy).not.toHaveBeenCalled();
    expect(host.textContent).toContain("绑定码无效或已过期");
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
  });
});
describe("group members shortcut", () => {
  it("opens native member details and the native add dialog rather than managing members in global settings", async () => {
    const inspect = vi.fn();
    const edit = vi.fn();
    await render(
      <GroupMembersMenu
        groupId="g_a"
        actors={[lead]}
        readOnly={false}
        onOpenActor={inspect}
        onEditActor={edit}
      />,
    );
    await click('[data-t05-change="members-entry"]');
    for (const region of document.querySelectorAll("[data-t05-change]"))
      expect(region.querySelector("[data-t05-mark] circle")).not.toBeNull();
    await click('[data-t05-change="member-details"]');
    expect(inspect).toHaveBeenCalledWith("lead");
    await click('[data-t05-change="members-entry"]');
    await click('[data-t05-change="add-member"]');
    expect(mocks.openModal).toHaveBeenCalledWith("addActor");
    expect(mocks.setRole).toHaveBeenCalledWith("peer");
    expect(edit).not.toHaveBeenCalled();
  });
  it("changes the native editor draft only and does not act on a stale group", async () => {
    const edit = vi.fn();
    await render(
      <GroupMembersMenu
        groupId="g_a"
        actors={[lead]}
        readOnly={false}
        onOpenActor={vi.fn()}
        onEditActor={edit}
      />,
    );
    await click('[data-t05-change="members-entry"]');
    await click('[data-t05-change="change-foreman"]');
    await click('[data-t05-change="foreman-local"]');
    expect(edit).toHaveBeenCalledWith(lead);
    expect(mocks.setRuntime).toHaveBeenCalledWith("opencode");
    const pending =
      deferred<
        ReturnType<
          typeof ok<{
            runtimes: { name: string; available: boolean; recommended_command: string }[];
          }>
        >
      >();
    mocks.fetchRuntimes.mockReturnValue(pending.promise);
    await click('[data-t05-change="members-entry"]');
    await click('[data-t05-change="change-foreman"]');
    await click('[data-t05-change="foreman-local"]');
    mocks.group.selectedGroupId = "g_b";
    await act(async () =>
      pending.resolve(
        ok({ runtimes: [{ name: "opencode", available: true, recommended_command: "opencode" }] }),
      ),
    );
    await wait();
    expect(edit).toHaveBeenCalledTimes(1);
  });
});
