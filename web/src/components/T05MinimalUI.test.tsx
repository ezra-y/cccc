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
  sharedWebModelBrowser: vi.fn(),
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
  getSharedWebModelBrowserWebSocketUrl: () => "ws://local.invalid/shared-preview",
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
  vi.stubGlobal(
    "confirm",
    vi.fn(() => true),
  );
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
  mocks.sharedWebModelBrowser.mockResolvedValue(
    ok({
      browser_session: {
        active: true,
        ready: true,
        login_required: false,
        tab_url: "https://chatgpt.com/c/shared-live",
      },
      browser_surface: { active: true, state: "ready" },
    }),
  );
  mocks.revokeWebModelConnector.mockResolvedValue(ok({ revoked: true }));
  mocks.bindCurrentWebModelBrowserConversation.mockResolvedValue(
    ok({ browser_session: session("g_a") }),
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
  vi.unstubAllGlobals();
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
    expect(host.querySelector("[data-t05-mark] circle")).toBeNull();
    expect(host.querySelector('[data-t05-review="group-selector"]')).not.toBeNull();
    expect(host.querySelector('[data-t05-review="chat-binding"]')).not.toBeNull();
    expect(
      host.querySelector('[data-t05-change="group-return-target"]')?.closest("[data-t05-review]"),
    ).toBeNull();
    expect(host.querySelectorAll("[data-t05-review]").length).toBeLessThanOrEqual(5);
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
    expect(document.querySelector('[data-t05-review="members-entry"]')).not.toBeNull();
    expect(document.querySelector('[data-t05-review="members-menu"]')).not.toBeNull();
    expect(
      document.querySelector('[data-t05-change="member-details"]')?.hasAttribute("data-t05-review"),
    ).toBe(false);
    expect(
      document
        .querySelector('[data-t05-change="members-menu"]')
        ?.classList.contains("t05-members-menu"),
    ).toBe(true);
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

describe("shared login, role, and confirmation ownership", () => {
  it.each([false, true])(
    "retains the upstream three steps and inserts group selection before connection (dark=%s)",
    async (isDark) => {
      await render(<WebModelConnectorsTab isDark={isDark} currentGroupId="g_a" />);
      const account = find('[data-setup-step="account"]');
      const connection = find('[data-setup-step="connection"]');
      const target = find('[data-setup-step="target"]');
      const selector = find('[data-t05-change="web-group-selector"]');
      expect(account.parentElement).toBe(connection.parentElement);
      expect(connection.parentElement).toBe(target.parentElement);
      expect(account.textContent).toContain("1. 登录 ChatGPT");
      expect(connection.textContent).toContain("2. 连接 CCCC MCP app");
      expect(target.textContent).toContain("3. 选择投递目标");
      expect(
        account.compareDocumentPosition(selector) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
      expect(
        selector.compareDocumentPosition(connection) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
      expect(
        connection.compareDocumentPosition(target) & Node.DOCUMENT_POSITION_FOLLOWING,
      ).toBeTruthy();
      expect(account.querySelector('[data-t05-change="copy-binding"]')).toBeNull();
      expect(connection.querySelector('[data-t05-change="copy-binding"]')).not.toBeNull();
      expect(connection.querySelector('[data-t05-change="legacy-setup"]')).not.toBeNull();
      expect(target.querySelector('[data-t05-change="save-return-target"]')).not.toBeNull();
      expect(account.textContent).not.toMatch(/共享|共用/);
      const select = find("#t05-web-group") as HTMLSelectElement;
      expect(select.style.colorScheme).toBe(isDark ? "dark" : "light");
      expect(select.style.paddingInlineEnd).toBe("3rem");
      expect(find('[data-testid="web-group-chevron"]').classList.contains("end-4")).toBe(true);
      await choose("g_b");
      expect(select.value).toBe("g_b");
      expect(find('[data-testid="shared-login-status"]').textContent).toBeTruthy();
      expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
      expect(mocks.openWebModelBrowserSurfaceSession).not.toHaveBeenCalled();
    },
  );

  it("uses the current browser conversation as a draft and binds a return target only after Save", async () => {
    await render();
    const url = find('input[placeholder="https://chatgpt.com/c/..."]') as HTMLInputElement;
    expect(url.value).toBe("https://chatgpt.com/c/g_a");
    await click('[data-testid="use-current-browser-chat"]');
    expect(url.value).toBe("https://chatgpt.com/c/shared-live");
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    expect(mocks.createWebModelConnectorBinding).not.toHaveBeenCalled();
    vi.mocked(window.confirm).mockReturnValue(false);
    await click('[data-t05-change="save-return-target"]');
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    vi.mocked(window.confirm).mockReturnValue(true);
    await click('[data-t05-change="save-return-target"]');
    expect(mocks.bindCurrentWebModelBrowserConversation).toHaveBeenCalledExactlyOnceWith({
      groupId: "g_a",
      actorId: "lead",
      conversationUrl: "https://chatgpt.com/c/shared-live",
      newChat: false,
    });
    expect(mocks.createWebModelConnectorBinding).not.toHaveBeenCalled();
    expect(
      mocks.sharedWebModelBrowser.mock.calls.every((call) => !["open", "close"].includes(call[0])),
    ).toBe(true);
  });

  it("renders one shared login before group selection and keeps it usable with no groups", async () => {
    mocks.fetchGroups.mockResolvedValue(ok({ groups: [] }));
    await render();
    const shared = find('[data-setup-step="account"]');
    const selector = find('[data-t05-change="web-group-selector"]');
    expect(
      shared.compareDocumentPosition(selector) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(shared.textContent).toContain("登录一次，所有工作组都使用这个账号");
    await click('[data-t05-change="open-shared-browser"]');
    expect(mocks.sharedWebModelBrowser).toHaveBeenCalledWith("open", {
      width: 1366,
      height: 900,
      inspect: true,
    });
    expect(mocks.openWebModelBrowserSurfaceSession).not.toHaveBeenCalled();
    expect(mocks.createWebModelConnector).not.toHaveBeenCalled();
  });
  it("shows a web peer under a local leader and never takes group connection status as shared login", async () => {
    const peer = { ...lead, id: "web-peer", role: "peer", title: "网页组员" } as Actor;
    mocks.fetchActors.mockImplementation(async (gid: string) =>
      ok({
        actors: gid === "g_a" ? [lead] : [{ ...lead, id: "local-lead", runtime: "opencode" }, peer],
      }),
    );
    mocks.fetchWebModelBrowserSession.mockResolvedValue(
      ok({ browser_session: { active: false, ready: false, login_required: true } }),
    );
    await render();
    const before = find('[data-testid="shared-login-status"]').textContent;
    await choose("g_b");
    expect(find('[data-testid="shared-login-status"]').textContent).toBe(before);
    expect(find('[data-testid="web-member-role"]').textContent).toContain("网页组员 · 组员");
    expect(
      mocks.sharedWebModelBrowser.mock.calls.every(
        (call) => call.length === 0 || call[0] === "status",
      ),
    ).toBe(true);
  });
  it("requires confirmation before disconnecting exactly one member, without logging out the shared browser", async () => {
    await render();
    await choose("g_b");
    vi.mocked(window.confirm).mockReturnValue(false);
    await click('[data-t05-change="disconnect-chat"]');
    expect(window.confirm).toHaveBeenLastCalledWith(expect.stringContaining("乙组"));
    expect(mocks.revokeWebModelConnector).not.toHaveBeenCalled();
    vi.mocked(window.confirm).mockReturnValue(true);
    await click('[data-t05-change="disconnect-chat"]');
    expect(mocks.revokeWebModelConnector).toHaveBeenCalledExactlyOnceWith("conn-g_b");
    expect(mocks.sharedWebModelBrowser.mock.calls.some((call) => call[0] === "close")).toBe(false);
  });
  it("cancels replacement-code generation before even issuing a new one", async () => {
    await render();
    vi.mocked(window.confirm).mockReturnValue(false);
    await click('[data-t05-change="copy-binding"]');
    expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining("另一条聊天成功使用该码"));
    expect(mocks.createWebModelConnectorBinding).not.toHaveBeenCalled();
    expect(mocks.copy).not.toHaveBeenCalled();
  });
  it("confirms changed return targets on save, but not while selecting the draft", async () => {
    await render();
    const newChat = host.querySelectorAll<HTMLInputElement>(
      'input[name="chatgpt-delivery-target"]',
    )[1];
    await act(async () => newChat.click());
    await wait();
    expect(window.confirm).not.toHaveBeenCalled();
    vi.mocked(window.confirm).mockReturnValue(false);
    await click('[data-t05-change="save-return-target"]');
    expect(window.confirm).toHaveBeenCalledWith(
      expect.stringContaining("https://chatgpt.com/c/g_a"),
    );
    expect(mocks.bindCurrentWebModelBrowserConversation).not.toHaveBeenCalled();
    expect(newChat.checked).toBe(true);
    await choose("g_b");
    expect((find("#t05-web-group") as HTMLSelectElement).value).toBe("g_a");
    vi.mocked(window.confirm).mockReturnValue(true);
    await click('[data-t05-change="save-return-target"]');
    expect(mocks.bindCurrentWebModelBrowserConversation).toHaveBeenCalledWith(
      expect.objectContaining({ groupId: "g_a", actorId: "lead", newChat: true }),
    );
  });
  it("warns that restart and close affect all groups; cancelling never closes the browser", async () => {
    await render();
    await click('[data-t05-change="preview-toggle"]');
    vi.mocked(window.confirm).mockReturnValue(false);
    await click('[data-t05-change="restart-shared-browser"]');
    expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining("所有组"));
    await click('[data-t05-change="close-shared-browser"]');
    expect(window.confirm).toHaveBeenLastCalledWith(expect.stringContaining("所有使用它的工作组"));
    expect(
      mocks.sharedWebModelBrowser.mock.calls.some(
        (call) => call[0] === "close" || call[0] === "open",
      ),
    ).toBe(false);
  });
});
