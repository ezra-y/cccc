import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Actor, GroupMeta, RemoteAccessState } from "../../../types";
import * as api from "../../../services/api";
import { copyTextToClipboard } from "../../../utils/copy";
import {
  isChatGptConversationUrl,
  liveBrowserConversationUrlFromSession,
  savedTargetDraftFromSession,
  targetDraftMatchesSaved,
} from "../../../utils/webModelTargetDraft";
import type { TargetDraftMode } from "../../../utils/webModelTargetDraft";
import {
  matchesWebModelActorSelection,
  resolveWebModelActorSelection,
} from "../../../utils/webModelSelection";
import { webModelConnectorMcpUrl } from "../../../utils/webModelConnector";
import { ProjectedBrowserSurfacePanel } from "../../browser/ProjectedBrowserSurfacePanel";
import { CheckIcon, ChevronDownIcon, PlusIcon } from "../../Icons";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import {
  dangerButtonClass,
  inputClass,
  labelClass,
  primaryButtonClass,
  secondaryButtonClass,
  settingsWorkspaceBodyClass,
  settingsWorkspaceHeaderClass,
  settingsWorkspacePanelClass,
  settingsWorkspaceShellClass,
} from "./types";

interface WebModelConnectorsTabProps {
  isDark: boolean;
  isActive?: boolean;
  currentGroupId?: string;
  refreshNonce?: number;
  onCreateGroup?: (groupId: string) => void;
  onCreateActor?: (
    groupId: string,
    preset: { role: "foreman" | "peer"; runtime?: "web_model" },
  ) => void;
  onEditActor?: (groupId: string, actorId: string, assistantKind: "web_model" | "local") => void;
  onOpenGuidance?: (groupId: string) => void;
  onOpenWebAccess?: () => void;
}

type TargetChoice = "current" | "pasted" | "new";
const DEFAULT_PROVIDER = "chatgpt_web";
const ACTOR_POLL_MS = 5_000;
type Translate = (key: string, options?: Record<string, unknown>) => string;

function isLocalConnectorUrl(url: string): boolean {
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.toLowerCase();
    return host === "localhost" || host === "127.0.0.1" || host === "::1" || host === "[::1]";
  } catch {
    return false;
  }
}

function isHttpsUrl(url: string): boolean {
  try {
    return new URL(url).protocol === "https:";
  } catch {
    return false;
  }
}

function formatTime(value?: string): string {
  if (!value) return "";
  try {
    return new Date(value).toLocaleString();
  } catch {
    return String(value || "");
  }
}

function normalized(value?: string | null): string {
  return String(value || "").trim();
}

function connectorActivityLabel(connector: api.WebModelConnector, wm: Translate): string {
  const status = String(connector.last_call_status || "").trim();
  const wait = String(connector.last_wait_status || "").trim();
  const tool = String(connector.last_tool_name || "").trim();
  if (!connector.last_activity_at) return wm("activity.notSeenYet");
  if (status === "error") return wm("activity.lastCallFailed");
  if (tool === "cccc_runtime_wait_next_turn" && wait) return wm("activity.wait", { status: wait });
  if (tool === "cccc_runtime_complete_turn" && wait)
    return wm("activity.complete", { status: wait });
  return tool || String(connector.last_method || "").trim() || wm("activity.seen");
}

function webModelQueuedCount(actor?: Actor | null): number {
  return Math.max(0, Number(actor?.web_model_queued_count || 0));
}

function isStandardChatGptWebModelActor(actor?: Actor | null): boolean {
  return (
    String(actor?.runtime || "")
      .trim()
      .toLowerCase() === "web_model" && !String(actor?.internal_kind || "").trim()
  );
}

function isForemanActor(actor?: Actor | null): boolean {
  return normalized(actor?.role).toLowerCase() === "foreman";
}

function browserSessionKey(groupId: string, actorId: string): string {
  return `${normalized(groupId)}::${normalized(actorId)}`;
}

function shortConversationLabel(url?: string): string {
  const value = normalized(url);
  if (!value) return "";
  try {
    const parsed = new URL(value);
    const parts = parsed.pathname.split("/").filter(Boolean);
    const cIndex = parts.findIndex((part) => part === "c");
    const conversationId = cIndex >= 0 ? parts[cIndex + 1] || "" : "";
    if (conversationId) {
      return `${parsed.hostname}/c/${conversationId.slice(0, 10)}…`;
    }
    return parsed.hostname;
  } catch {
    return value.length > 48 ? `${value.slice(0, 45)}…` : value;
  }
}

type SetupTone = "ready" | "needs" | "warn" | "neutral";

function setupPillClass(tone: SetupTone): string {
  if (tone === "ready")
    return "border-emerald-500/25 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300";
  if (tone === "needs")
    return "border-amber-500/25 bg-amber-500/10 text-amber-700 dark:text-amber-200";
  if (tone === "warn") return "border-rose-500/25 bg-rose-500/10 text-rose-700 dark:text-rose-300";
  return "border-[var(--glass-border-subtle)] bg-[var(--glass-tab-bg)] text-[var(--color-text-secondary)]";
}

function healthNextActionText(
  health: api.WebModelHealthSnapshot | null | undefined,
  wm: Translate,
): string {
  const action = health?.next_action;
  const recommended = String(action?.recommended || "none").trim();
  if (!recommended || recommended === "none") return "";
  const label = wm(`nextAction.${recommended}`, {
    defaultValue: String(action?.label || "").trim() || recommended,
  });
  const reason = wm(`nextActionReason.${recommended}`, {
    defaultValue: String(action?.reason || "").trim(),
  });
  return reason ? `${label}: ${reason}` : label;
}

export default function WebModelConnectorsTab({
  isDark,
  isActive = true,
  currentGroupId = "",
  refreshNonce = 0,
  onCreateGroup,
  onCreateActor,
  onEditActor,
  onOpenGuidance,
  onOpenWebAccess,
}: WebModelConnectorsTabProps) {
  const { t } = useTranslation("settings");
  const wm = useCallback<Translate>((key, options) => t(`webModels.chatgpt.${key}`, options), [t]);

  const [groups, setGroups] = useState<GroupMeta[]>([]);
  const [actors, setActors] = useState<Actor[]>([]);
  const [actorsLoading, setActorsLoading] = useState(false);
  const [connectors, setConnectors] = useState<api.WebModelConnector[]>([]);
  const [remoteState, setRemoteState] = useState<RemoteAccessState | null>(null);
  const [groupId, setGroupId] = useState(() => normalized(currentGroupId));
  const [actorId, setActorId] = useState("");
  const [pageBusy, setPageBusy] = useState(false);
  const [browserBusy, setBrowserBusy] = useState(false);
  const [createBusy, setCreateBusy] = useState(false);
  const [bindingBusy, setBindingBusy] = useState(false);
  const [revokeBusyId, setRevokeBusyId] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [browserSessionsByActor, setBrowserSessionsByActor] = useState<
    Record<string, api.WebModelBrowserSession>
  >({});
  const [showBrowserSurface, setShowBrowserSurface] = useState(false);
  const [foremanMenuOpen, setForemanMenuOpen] = useState(false);
  const [editorOpen, setEditorOpen] = useState(false);
  const [conversationUrlDraft, setConversationUrlDraft] = useState("");
  const [targetDraftMode, setTargetDraftMode] = useState<TargetDraftMode>("existing");
  const [targetDraftTouched, setTargetDraftTouched] = useState(false);
  const [targetChoice, setTargetChoice] = useState<TargetChoice>("pasted");
  const [targetChoiceTouched, setTargetChoiceTouched] = useState(false);
  const selectionRef = useRef({ groupId: normalized(currentGroupId), actorId: "" });

  useEffect(() => {
    selectionRef.current = { groupId: normalized(groupId), actorId: normalized(actorId) };
  }, [actorId, groupId]);

  const selectGroup = useCallback((nextGroupId: string) => {
    const gid = normalized(nextGroupId);
    const currentGid = normalized(selectionRef.current.groupId);
    if (gid && gid === currentGid) {
      return;
    }
    // Switching only changes this page's local selection; in-flight responses for
    // the previous group are validated against the ref below and discarded.
    selectionRef.current = { groupId: gid, actorId: "" };
    setGroupId(gid);
    setActors([]);
    setActorId("");
    setBrowserSessionsByActor({});
    setShowBrowserSurface(false);
    setForemanMenuOpen(false);
    setEditorOpen(false);
    setTargetDraftTouched(false);
    setTargetChoiceTouched(false);
    setTargetDraftMode("existing");
    setConversationUrlDraft("");
    setTargetChoice("pasted");
    setBrowserBusy(false);
    setError("");
    setNotice("");
  }, []);

  const loadConnectors = useCallback(async () => {
    const resp = await api.fetchWebModelConnectors();
    if (resp.ok) {
      setConnectors(resp.result?.connectors || []);
    } else {
      setError(resp.error?.message || wm("errors.loadConnectorsFailed"));
    }
  }, [wm]);

  const loadActorsForGroup = useCallback(
    async (gid: string, options?: { silent?: boolean }) => {
      const normalizedGroupId = normalized(gid);
      if (!normalizedGroupId) {
        setActors([]);
        setActorId("");
        return;
      }
      const resp = await api.fetchActors(normalizedGroupId, true, { noCache: true });
      if (normalized(selectionRef.current.groupId) !== normalizedGroupId) return;
      if (resp.ok) {
        const nextActors = resp.result?.actors || [];
        setActors(nextActors);
        setActorId((current) => {
          if (
            current &&
            nextActors.some(
              (actor) => actor.id === current && isStandardChatGptWebModelActor(actor),
            )
          )
            return current;
          const webForeman = nextActors.find(
            (actor) => isForemanActor(actor) && isStandardChatGptWebModelActor(actor),
          );
          return (
            normalized(webForeman?.id) ||
            normalized(nextActors.find((actor) => isStandardChatGptWebModelActor(actor))?.id) ||
            ""
          );
        });
      } else if (!options?.silent) {
        setActors([]);
        setActorId("");
        setError(resp.error?.message || wm("errors.loadActorsFailed"));
      }
    },
    [wm],
  );

  const storeSession = useCallback(
    (gid: string, aid: string, session: api.WebModelBrowserSession | null) => {
      const key = browserSessionKey(gid, aid);
      setBrowserSessionsByActor((current) => ({ ...current, [key]: session || {} }));
    },
    [],
  );

  const loadBrowserSession = useCallback(
    async (gid: string = groupId, aid: string = actorId, options?: { silent?: boolean }) => {
      const selection = resolveWebModelActorSelection(gid, aid);
      if (!selection) return;
      const resp = await api.fetchWebModelBrowserSession(selection.groupId, selection.actorId, {
        inspect: !options?.silent,
      });
      if (
        !matchesWebModelActorSelection(selectionRef.current, selection.groupId, selection.actorId)
      )
        return;
      if (resp.ok) {
        storeSession(selection.groupId, selection.actorId, resp.result?.browser_session || null);
        return true;
      } else if (!options?.silent) {
        setError(resp.error?.message || wm("errors.loadBrowserFailed"));
      }
    },
    [groupId, actorId, storeSession, wm],
  );

  const loadPage = useCallback(async () => {
    if (!isActive) return;
    setPageBusy(true);
    setError("");
    try {
      const [groupsResp, connectorsResp, remoteResp] = await Promise.all([
        api.fetchGroups(),
        api.fetchWebModelConnectors(),
        api.fetchRemoteAccessState(),
      ]);
      if (remoteResp.ok) setRemoteState(remoteResp.result?.remote_access || null);
      if (!connectorsResp.ok) {
        setError(connectorsResp.error?.message || wm("errors.loadConnectorsFailed"));
      } else {
        setConnectors(connectorsResp.result?.connectors || []);
      }
      if (!groupsResp.ok) {
        setError(groupsResp.error?.message || wm("errors.loadGroupsFailed"));
        return;
      }
      const nextGroups = groupsResp.result?.groups || [];
      setGroups(nextGroups);
      const preferred = normalized(currentGroupId);
      const selected = normalized(selectionRef.current.groupId);
      const nextGroupId = nextGroups.some((group) => normalized(group.group_id) === selected)
        ? selected
        : nextGroups.some((group) => normalized(group.group_id) === preferred)
          ? preferred
          : normalized(nextGroups[0]?.group_id);
      if (normalized(selectionRef.current.groupId) !== nextGroupId) {
        selectGroup(nextGroupId);
      }
    } catch {
      setError(wm("errors.loadGroupsFailed"));
    } finally {
      setPageBusy(false);
    }
  }, [currentGroupId, isActive, selectGroup, wm]);

  useEffect(() => {
    void loadPage();
  }, [loadPage, refreshNonce]);

  useEffect(() => {
    if (!isActive) {
      setBrowserSessionsByActor({});
      setShowBrowserSurface(false);
      return;
    }
    if (!normalized(groupId)) {
      setActors([]);
      setActorId("");
      setBrowserSessionsByActor({});
      setShowBrowserSurface(false);
      return;
    }
    let cancelled = false;
    setActorsLoading(true);
    void (async () => {
      await loadActorsForGroup(groupId);
      if (!cancelled) setActorsLoading(false);
    })();
    return () => {
      cancelled = true;
    };
  }, [groupId, isActive, loadActorsForGroup]);

  useEffect(() => {
    if (!isActive || !normalized(groupId) || !normalized(actorId)) {
      setTargetDraftTouched(false);
      setTargetChoiceTouched(false);
      return;
    }
    void loadBrowserSession(groupId, actorId);
  }, [actorId, groupId, isActive, loadBrowserSession]);

  useEffect(() => {
    if (!isActive || !normalized(groupId)) return;
    let pending = false;
    const timer = window.setInterval(() => {
      if (pending || document.hidden) return;
      pending = true;
      void Promise.all([
        loadActorsForGroup(groupId, { silent: true }),
        normalized(actorId)
          ? loadBrowserSession(groupId, actorId, { silent: true })
          : Promise.resolve(),
        loadConnectors(),
      ])
        .catch(() => {
          // Preserve the last known data; the next poll or explicit refresh can recover.
        })
        .finally(() => {
          pending = false;
        });
    }, ACTOR_POLL_MS);
    return () => window.clearInterval(timer);
  }, [actorId, groupId, isActive, loadActorsForGroup, loadBrowserSession, loadConnectors]);

  const loadSurfaceSession = useCallback(async () => {
    return api.fetchWebModelBrowserSurfaceSession(groupId, actorId, { inspect: true });
  }, [actorId, groupId]);

  const pushNotice = useCallback((value: string) => {
    setNotice(value);
    try {
      window.setTimeout(() => setNotice(""), 1600);
    } catch {
      // Timer support is not required for the notice.
    }
  }, []);

  const activeConnectors = useMemo(
    () => connectors.filter((connector) => !connector.revoked),
    [connectors],
  );
  const currentGroupActiveConnectors = useMemo(
    () => activeConnectors.filter((connector) => normalized(connector.group_id) === groupId),
    [activeConnectors, groupId],
  );
  const webModelActors = useMemo(
    () => actors.filter((actor) => isStandardChatGptWebModelActor(actor)),
    [actors],
  );
  const foremanActor = useMemo(
    () => actors.find((actor) => isForemanActor(actor)) || null,
    [actors],
  );
  const webForeman =
    foremanActor && isStandardChatGptWebModelActor(foremanActor) ? foremanActor : null;
  const selectedWebActor = useMemo(
    () => webModelActors.find((actor) => normalized(actor.id) === normalized(actorId)) || null,
    [webModelActors, actorId],
  );
  const selectedConnector = useMemo(() => {
    if (!selectedWebActor) return null;
    return (
      currentGroupActiveConnectors.find(
        (connector) => normalized(connector.actor_id) === selectedWebActor.id,
      ) || null
    );
  }, [currentGroupActiveConnectors, selectedWebActor]);
  const selectedSessionBound = Boolean(selectedConnector?.session_bound);
  const extraChatGptActors = webModelActors.slice(1);
  const queuedCount = webModelQueuedCount(selectedWebActor);
  const selectedMcpUrl = webModelConnectorMcpUrl(selectedConnector || null);
  const selectedConnectorUrl = normalized(selectedConnector?.connector_url);
  const selectedMcpUrlForValidation = selectedMcpUrl || selectedConnectorUrl;
  const mcpUrlLocalWarning =
    Boolean(selectedMcpUrlForValidation) && isLocalConnectorUrl(selectedMcpUrlForValidation);
  const mcpUrlHttpsWarning =
    Boolean(selectedMcpUrlForValidation) && !isHttpsUrl(selectedMcpUrlForValidation);
  const mcpLastCallFailed = normalized(selectedConnector?.last_call_status) === "error";
  const chatGptSeen = Boolean(selectedConnector?.last_activity_at);
  const configuredPublicUrl = normalized(
    remoteState?.config?.web_public_url || remoteState?.diagnostics?.web_public_url,
  );
  const publicEndpointReady = Boolean(configuredPublicUrl && isHttpsUrl(configuredPublicUrl));
  const accessTokenPresent = Boolean(
    remoteState?.config?.access_token_configured || remoteState?.diagnostics?.access_token_present,
  );
  const webAccessReady = publicEndpointReady && accessTokenPresent;

  const selectedBrowserSession =
    browserSessionsByActor[browserSessionKey(groupId, actorId)] || null;
  const selectedHealth = selectedBrowserSession?.health_snapshot || null;
  const deliveryTarget =
    selectedBrowserSession?.delivery_target || selectedHealth?.delivery_target || null;
  const deliveryTargetState = normalized(deliveryTarget?.state);
  const boundConversationUrl = normalized(selectedBrowserSession?.conversation_url);
  const pendingNewChatBind = Boolean(selectedBrowserSession?.pending_new_chat_bind);
  const deliveryTargetSavedAt = normalized(
    deliveryTarget?.saved_at || selectedBrowserSession?.target_saved_at,
  );
  const currentBrowserConversationUrl =
    liveBrowserConversationUrlFromSession(selectedBrowserSession);
  const browserActive = Boolean(selectedBrowserSession?.active);
  const browserReady = Boolean(selectedBrowserSession?.ready);
  const browserStatusLabel = browserReady
    ? wm("browser.ready")
    : selectedBrowserSession?.login_required
      ? wm("browser.signInNeeded")
      : browserActive
        ? wm("browser.open")
        : wm("browser.notOpen");
  const lastDeliveryText = normalized(selectedBrowserSession?.last_delivery_at)
    ? formatTime(selectedBrowserSession.last_delivery_at)
    : "";

  const nextActionText = healthNextActionText(selectedHealth, wm);
  const savedTargetDraft = savedTargetDraftFromSession(selectedBrowserSession);
  const returnBound = Boolean(boundConversationUrl);
  const returnNewChat =
    !returnBound &&
    (pendingNewChatBind ||
      deliveryTargetState === "new_chat_armed" ||
      deliveryTargetState === "new_chat_submitted");
  const returnStatusKey = returnBound
    ? "groupDetail.returnSaved"
    : returnNewChat
      ? "groupDetail.returnNewChat"
      : "groupDetail.returnNotSaved";

  const targetDraftUrl = normalized(conversationUrlDraft);
  const targetDraftMatchesSavedTarget = targetDraftMatchesSaved({
    mode: targetDraftMode,
    url: targetDraftUrl,
    saved: savedTargetDraft,
  });
  const targetDraftDirty = !targetDraftMatchesSavedTarget;
  const targetDraftError =
    targetDraftMode === "existing" && targetDraftDirty && !isChatGptConversationUrl(targetDraftUrl)
      ? wm("editor.urlInvalid")
      : "";
  const targetSaveDisabled =
    browserBusy ||
    !normalized(groupId) ||
    !normalized(actorId) ||
    Boolean(targetDraftError) ||
    !targetDraftDirty;

  const targetRadioClass = (choice: TargetChoice) =>
    [
      "flex cursor-pointer items-start gap-2 rounded-md border px-3 py-2 text-left text-sm transition-colors",
      targetChoice === choice
        ? "border-[var(--color-text-primary)] bg-[var(--glass-tab-bg-hover)] text-[var(--color-text-primary)]"
        : "border-[var(--glass-border-subtle)] bg-[var(--glass-panel-bg)] text-[var(--color-text-secondary)] hover:bg-[var(--glass-tab-bg-hover)] hover:text-[var(--color-text-primary)]",
    ].join(" ");

  const chooseTargetChoice = (choice: TargetChoice, mode: TargetDraftMode) => {
    setTargetChoice(choice);
    setTargetChoiceTouched(true);
    if (targetDraftMode !== mode) setTargetDraftMode(mode);
    setTargetDraftTouched(true);
  };

  const readCurrentChat = async () => {
    const gid = groupId;
    const aid = actorId;
    setBrowserBusy(true);
    setError("");
    try {
      const resp = await api.fetchWebModelBrowserSession(gid, aid, { inspect: false });
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (!resp.ok) {
        setError(resp.error.message);
        return;
      }
      const session = resp.result?.browser_session || null;
      const url = liveBrowserConversationUrlFromSession(session);
      if (!isChatGptConversationUrl(url)) {
        setError(wm("editor.currentChatUnavailable"));
        return;
      }
      storeSession(gid, aid, session);
      setTargetChoice("current");
      setTargetChoiceTouched(true);
      setTargetDraftMode("existing");
      setConversationUrlDraft(url);
      setTargetDraftTouched(true);
      pushNotice(wm("notices.currentChatRead"));
    } catch {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
        setError(wm("errors.loadBrowserFailed"));
    } finally {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid)) setBrowserBusy(false);
    }
  };

  const cancelTargetEdit = () => {
    const saved = savedTargetDraftFromSession(selectedBrowserSession);
    setTargetDraftMode(saved.mode);
    setConversationUrlDraft(saved.url);
    setTargetChoice(saved.mode === "new" ? "new" : "pasted");
    setTargetDraftTouched(false);
    setTargetChoiceTouched(false);
    pushNotice(wm("notices.targetEditCancelled"));
  };

  useEffect(() => {
    if (targetDraftTouched) return;
    const draft = savedTargetDraftFromSession(selectedBrowserSession);
    setTargetDraftMode(draft.mode);
    setConversationUrlDraft(draft.url);
  }, [selectedBrowserSession, targetDraftTouched]);

  useEffect(() => {
    if (targetDraftTouched || targetChoiceTouched) return;
    setTargetChoice(savedTargetDraft.mode === "new" ? "new" : "pasted");
  }, [savedTargetDraft, targetChoiceTouched, targetDraftTouched]);

  const createConnectorManual = async () => {
    const gid = normalized(groupId);
    const aid = normalized(actorId);
    if (!gid || !aid) {
      setError(wm("errors.selectActorFirst"));
      return;
    }
    setCreateBusy(true);
    setError("");
    try {
      const targetActor = webModelActors.find((item) => normalized(item.id) === aid);
      const resp = await api.createWebModelConnector({
        groupId: gid,
        actorId: aid,
        provider: DEFAULT_PROVIDER,
        label: normalized(targetActor?.title || targetActor?.id || aid),
      });
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (resp.ok) {
        pushNotice(
          resp.result?.replaced_connector_ids?.length
            ? wm("notices.connectorRotated")
            : wm("notices.connectorCreated"),
        );
        await loadConnectors();
      } else {
        setError(resp.error?.message || wm("errors.createConnectorFailed"));
      }
    } catch {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
        setError(wm("errors.createConnectorFailed"));
    } finally {
      setCreateBusy(false);
    }
  };

  const revokeConnector = async (connectorId: string) => {
    const cid = normalized(connectorId);
    const gid = groupId;
    const aid = actorId;
    if (!cid) return;
    setRevokeBusyId(cid);
    setError("");
    try {
      const resp = await api.revokeWebModelConnector(cid);
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (resp.ok) {
        await Promise.all([loadConnectors(), loadBrowserSession(gid, aid)]);
        if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
          pushNotice(wm("notices.chatDisconnected"));
      } else setError(resp.error?.message || wm("errors.revokeConnectorFailed"));
    } catch {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
        setError(wm("errors.revokeConnectorFailed"));
    } finally {
      setRevokeBusyId("");
    }
  };

  const copyValue = async (value: string, labelText: string) => {
    const ok = await copyTextToClipboard(value);
    pushNotice(ok ? wm("notices.copied", { label: labelText }) : wm("notices.copyFailed"));
  };

  const openBrowserLogin = async () => {
    const gid = normalized(groupId);
    const aid = normalized(actorId);
    setBrowserBusy(true);
    setError("");
    try {
      const selection = resolveWebModelActorSelection(gid, aid);
      if (!selection) {
        setError(wm("errors.selectActorFirst"));
        return;
      }
      const resp = await api.openWebModelBrowserSession({
        groupId: gid,
        actorId: aid,
        visibility: "visible",
      });
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (!resp.ok) {
        setError(resp.error?.message || wm("errors.openBrowserFailed"));
        return;
      }
      storeSession(gid, aid, resp.result?.browser_session || null);
      const readback = await api.fetchWebModelBrowserSession(gid, aid, { inspect: true });
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (readback.ok) {
        storeSession(gid, aid, readback.result?.browser_session || null);
        setShowBrowserSurface(true);
        pushNotice(wm("notices.signInOpened"));
      } else {
        setError(readback.error?.message || wm("errors.loadBrowserFailed"));
      }
    } catch {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
        setError(wm("errors.openBrowserFailed"));
    } finally {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid)) setBrowserBusy(false);
    }
  };

  const checkBrowserStatus = async () => {
    setBrowserBusy(true);
    setError("");
    try {
      if (await loadBrowserSession(groupId, actorId)) pushNotice(wm("notices.browserChecked"));
    } finally {
      setBrowserBusy(false);
    }
  };

  const closeBrowser = async () => {
    const gid = normalized(groupId);
    const aid = normalized(actorId);
    setBrowserBusy(true);
    setError("");
    try {
      if (!resolveWebModelActorSelection(gid, aid)) {
        setError(wm("errors.selectActorFirst"));
        return;
      }
      const resp = await api.closeWebModelBrowserSession(gid, aid);
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (resp.ok) {
        storeSession(gid, aid, resp.result?.browser_session || null);
        pushNotice(wm("notices.browserClosed"));
      } else {
        setError(resp.error?.message || wm("errors.closeBrowserFailed"));
      }
    } catch {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
        setError(wm("errors.closeBrowserFailed"));
    } finally {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid)) setBrowserBusy(false);
    }
  };

  const bindConversation = useCallback(
    async (conversationUrl: string, options?: { newChat?: boolean; notice?: string }) => {
      const gid = normalized(groupId);
      const aid = normalized(actorId);
      if (!resolveWebModelActorSelection(gid, aid)) return;
      setBrowserBusy(true);
      setError("");
      try {
        const resp = await api.bindCurrentWebModelBrowserConversation({
          groupId: gid,
          actorId: aid,
          conversationUrl,
          newChat: Boolean(options?.newChat),
        });
        if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
        if (resp.ok) {
          const session = resp.result?.browser_session || null;
          storeSession(gid, aid, session);
          if (matchesWebModelActorSelection(selectionRef.current, gid, aid)) {
            const draft = savedTargetDraftFromSession(session);
            setTargetDraftMode(draft.mode);
            setConversationUrlDraft(draft.url);
            setTargetChoice(draft.mode === "new" ? "new" : "pasted");
            setTargetDraftTouched(false);
            setTargetChoiceTouched(false);
            pushNotice(
              options?.notice ||
                (options?.newChat
                  ? wm("notices.targetSavedNewChat")
                  : wm("notices.targetSavedExisting")),
            );
          }
        } else {
          setError(resp.error?.message || wm("errors.bindConversationFailed"));
        }
      } catch {
        if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
          setError(wm("errors.bindConversationFailed"));
      } finally {
        if (matchesWebModelActorSelection(selectionRef.current, gid, aid)) setBrowserBusy(false);
      }
    },
    [actorId, groupId, storeSession, pushNotice, wm],
  );

  const saveDeliveryTarget = async () => {
    if (targetSaveDisabled) return;
    if (targetDraftMode === "new") {
      await bindConversation("https://chatgpt.com/", {
        newChat: true,
        notice: wm("notices.targetSavedNewChat"),
      });
      return;
    }
    await bindConversation(targetDraftUrl, { notice: wm("notices.targetSavedExisting") });
  };

  const copyConnectionInstructions = async () => {
    const gid = normalized(groupId);
    const aid = normalized(actorId);
    if (!resolveWebModelActorSelection(gid, aid)) {
      setError(wm("errors.selectActorFirst"));
      return;
    }
    setBindingBusy(true);
    setError("");
    try {
      let connector: api.WebModelConnector | null = selectedConnector;
      if (!connector) {
        const createResp = await api.createWebModelConnector({
          groupId: gid,
          actorId: aid,
          provider: DEFAULT_PROVIDER,
          label: normalized(selectedWebActor?.title || selectedWebActor?.id || aid),
        });
        if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
        if (!createResp.ok) {
          setError(createResp.error?.message || wm("errors.bindingRequestFailed"));
          return;
        }
        connector = createResp.result?.connector || null;
        await loadConnectors();
      }
      if (!connector) return;
      const cid = normalized(connector.connector_id);
      if (!cid) {
        setError(wm("errors.bindingRequestFailed"));
        return;
      }
      const resp = await api.createWebModelConnectorBinding(cid);
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      if (!resp.ok) {
        setError(resp.error?.message || wm("errors.bindingRequestFailed"));
        return;
      }
      const code = normalized(resp.result?.code);
      if (
        !code ||
        normalized(resp.result?.group_id) !== gid ||
        normalized(resp.result?.actor_id) !== aid ||
        !Number.isFinite(Date.parse(resp.result.binding_expires_at)) ||
        Date.parse(resp.result.binding_expires_at) <= Date.now()
      ) {
        setError(wm("errors.bindingRequestFailed"));
        return;
      }
      const copied = await copyTextToClipboard(
        wm("binding.template", {
          code: JSON.stringify(code),
          group: groups.find((group) => normalized(group.group_id) === gid)?.title || gid,
          actor: aid,
          interpolation: { escapeValue: false },
        }),
      );
      if (!matchesWebModelActorSelection(selectionRef.current, gid, aid)) return;
      pushNotice(copied ? wm("notices.bindingCopied") : wm("notices.copyFailed"));
    } catch {
      if (matchesWebModelActorSelection(selectionRef.current, gid, aid))
        setError(wm("errors.bindingRequestFailed"));
    } finally {
      setBindingBusy(false);
    }
  };

  const groupRowStatus = (gid: string) => {
    const rows = activeConnectors.filter((connector) => normalized(connector.group_id) === gid);
    if (rows.some((connector) => connector.session_bound)) return wm("groups.identityBound");
    if (rows.length) return wm("groups.urlReady");
    return wm("groups.notConnected");
  };

  const referenceStatuses = [
    {
      label: wm("reference.webAccess"),
      value: webAccessReady ? wm("reference.webAccessReady") : wm("reference.webAccessNeedsSetup"),
      ready: webAccessReady,
    },
    {
      label: wm("reference.member"),
      value: !selectedWebActor
        ? wm("reference.memberMissing")
        : extraChatGptActors.length
          ? wm("reference.memberDuplicate", { count: webModelActors.length })
          : selectedWebActor.running
            ? wm("reference.memberRunning")
            : wm("reference.memberStopped"),
      ready: Boolean(selectedWebActor?.running && !extraChatGptActors.length),
    },
    {
      label: wm("reference.chatgpt"),
      value: browserReady
        ? wm("reference.chatgptSignedIn")
        : browserActive
          ? wm("reference.chatgptSignInNeeded")
          : wm("reference.chatgptClosed"),
      ready: browserReady,
    },
    {
      label: wm("reference.mcpApp"),
      value: selectedSessionBound
        ? wm("reference.mcpBound")
        : !selectedConnector
          ? wm("reference.mcpNotCreated")
          : !selectedMcpUrl
            ? wm("reference.mcpNeedsRotation")
            : mcpLastCallFailed
              ? wm("reference.mcpLastCallFailed")
              : chatGptSeen
                ? wm("reference.mcpSeenAt", {
                    time: formatTime(selectedConnector?.last_activity_at),
                  })
                : wm("reference.mcpWaitingFirstCall"),
      ready: selectedSessionBound || (Boolean(selectedMcpUrl) && chatGptSeen && !mcpLastCallFailed),
    },
    {
      label: wm("reference.returnTarget"),
      value: returnBound
        ? wm("reference.returnSaved")
        : returnNewChat
          ? wm("reference.returnNewChat")
          : wm("reference.returnNone"),
      ready: returnBound || returnNewChat,
    },
  ];

  return (
    <div className={settingsWorkspaceShellClass(isDark)}>
      <div className={settingsWorkspaceHeaderClass(isDark)}>
        <div className="min-w-0">
          <h3 className="text-base font-semibold text-[var(--color-text-primary)]">
            {wm("title")}
          </h3>
          <p className="mt-1 max-w-3xl text-sm leading-6 text-[var(--color-text-tertiary)]">
            {wm("description")}
          </p>
        </div>
        <button
          type="button"
          onClick={() => void loadPage()}
          disabled={pageBusy}
          className={`${secondaryButtonClass("sm")} shrink-0 whitespace-nowrap`}
        >
          {wm("buttons.refresh")}
        </button>
      </div>

      <div className={settingsWorkspaceBodyClass}>
        {error ? (
          <div
            role="alert"
            className="rounded-lg border border-rose-500/30 bg-rose-500/10 px-3 py-2 text-sm text-rose-700 dark:text-rose-300"
          >
            {error}
          </div>
        ) : null}
        {notice ? (
          <div
            role="status"
            className="rounded-lg border border-emerald-500/25 bg-emerald-500/10 px-3 py-2 text-sm text-emerald-700 dark:text-emerald-300"
          >
            {notice}
          </div>
        ) : null}

        <details
          data-testid="web-model-original-reference"
          className={settingsWorkspacePanelClass(isDark)}
        >
          <summary className="cursor-pointer text-sm font-semibold text-[var(--color-text-primary)]">
            <span>{wm("reference.title")}</span>
            <span className="mt-1 block text-xs font-normal leading-5 text-[var(--color-text-tertiary)]">
              {wm("reference.description")}
            </span>
          </summary>
          <div className="mt-4 space-y-5 border-t border-[var(--glass-border-subtle)] pt-4">
            <div>
              <div className="text-sm font-semibold text-[var(--color-text-primary)]">
                {wm("reference.summaryTitle")}
              </div>
              <dl className="mt-3 grid gap-3 sm:grid-cols-2 2xl:grid-cols-3">
                {referenceStatuses.map((item) => (
                  <div key={item.label} className="min-w-0">
                    <dt className="text-xs text-[var(--color-text-muted)]">{item.label}</dt>
                    <dd
                      className={`mt-1 inline-flex max-w-full rounded-full border px-2 py-0.5 text-xs font-semibold ${
                        item.ready
                          ? "border-emerald-500/25 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
                          : "border-amber-500/25 bg-amber-500/10 text-amber-700 dark:text-amber-300"
                      }`}
                    >
                      <span className="truncate">{item.value}</span>
                    </dd>
                  </div>
                ))}
              </dl>
              <p className="mt-3 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {wm("reference.webAccessNote")}
              </p>
            </div>

            <div className="border-t border-[var(--glass-border-subtle)] pt-4">
              <div className="text-sm font-semibold text-[var(--color-text-primary)]">
                {wm("reference.mcpTitle")}
              </div>
              <p className="mt-1 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {selectedMcpUrl
                  ? wm("reference.mcpCopyHint")
                  : selectedConnector
                    ? wm("reference.mcpRotateHint")
                    : wm("reference.mcpCreateHint")}
              </p>
              {!chatGptSeen ? (
                <ol className="mt-3 list-decimal space-y-1 pl-4 text-xs leading-5 text-[var(--color-text-tertiary)]">
                  <li>{wm("reference.instructionOpenSettings")}</li>
                  <li>{wm("reference.instructionCreateApp")}</li>
                  <li>{wm("reference.instructionEnableConnector")}</li>
                </ol>
              ) : null}
              <div className="mt-3 rounded-md border border-amber-500/20 bg-amber-500/5 px-3 py-2 text-xs leading-5 text-amber-800 dark:text-amber-200">
                <span>{wm("reference.permissionHint")}</span>
                <a
                  href="https://help.openai.com/en/articles/11487775-apps-in-chatgpt"
                  target="_blank"
                  rel="noreferrer"
                  className="ml-2 font-semibold underline-offset-2 hover:underline"
                >
                  {wm("reference.permissionDocsLink")}
                </a>
              </div>
              {selectedConnector && !selectedMcpUrl ? (
                <div className="mt-2 text-xs leading-5 text-amber-700 dark:text-amber-300">
                  {wm("reference.warningRotate")}
                </div>
              ) : null}
              {mcpUrlLocalWarning ? (
                <div className="mt-2 text-xs leading-5 text-amber-700 dark:text-amber-300">
                  {wm("reference.warningLocalUrl")}
                </div>
              ) : null}
              {mcpUrlHttpsWarning && !mcpUrlLocalWarning ? (
                <div className="mt-2 text-xs leading-5 text-amber-700 dark:text-amber-300">
                  {wm("reference.warningNonHttps")}
                </div>
              ) : null}
              <div className="mt-3 flex flex-wrap gap-2">
                {selectedMcpUrl ? (
                  <button
                    type="button"
                    onClick={() => void copyValue(selectedMcpUrl, wm("copyLabels.mcpUrl"))}
                    className={secondaryButtonClass("sm")}
                  >
                    {wm("buttons.copyMcpUrl")}
                  </button>
                ) : (
                  <button
                    type="button"
                    onClick={() => void createConnectorManual()}
                    disabled={createBusy || !normalized(actorId)}
                    className={primaryButtonClass(createBusy)}
                  >
                    {selectedConnector ? wm("buttons.rotateMcpUrl") : wm("buttons.createMcpUrl")}
                  </button>
                )}
                {selectedConnector ? (
                  <button
                    type="button"
                    onClick={() => void revokeConnector(selectedConnector.connector_id)}
                    disabled={revokeBusyId === normalized(selectedConnector.connector_id)}
                    className={dangerButtonClass("sm")}
                  >
                    {wm("buttons.revokeMcpUrl")}
                  </button>
                ) : null}
                {onOpenWebAccess ? (
                  <button
                    type="button"
                    onClick={onOpenWebAccess}
                    className={secondaryButtonClass("sm")}
                  >
                    {wm("buttons.openWebAccess")}
                  </button>
                ) : null}
              </div>
              <p className="mt-3 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {wm("reference.optionalNote")}
              </p>
            </div>
          </div>
        </details>

        <section
          data-testid="web-model-shared-browser"
          className={settingsWorkspacePanelClass(isDark)}
          aria-labelledby="web-model-shared-browser-title"
        >
          <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
            <div className="min-w-0">
              <h4
                id="web-model-shared-browser-title"
                className="text-sm font-semibold text-[var(--color-text-primary)]"
              >
                {wm("browser.title")}
              </h4>
              <p className="mt-1 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {wm("browser.description")}
              </p>
              <p className="mt-1 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {wm("browser.hideNote")}
              </p>
            </div>
            <div className="flex shrink-0 flex-wrap gap-2 sm:justify-end">
              <button
                type="button"
                data-testid="web-model-open-login"
                onClick={() => void openBrowserLogin()}
                disabled={browserBusy || !normalized(actorId)}
                className={
                  browserReady ? secondaryButtonClass("sm") : primaryButtonClass(browserBusy)
                }
              >
                {wm("buttons.openLogin")}
              </button>
              {showBrowserSurface ? (
                <button
                  type="button"
                  data-testid="web-model-hide-surface"
                  onClick={() => setShowBrowserSurface(false)}
                  className={secondaryButtonClass("sm")}
                >
                  {wm("buttons.hideSurface")}
                </button>
              ) : (
                <button
                  type="button"
                  data-testid="web-model-view-surface"
                  onClick={() => setShowBrowserSurface(true)}
                  disabled={!normalized(actorId)}
                  className={secondaryButtonClass("sm")}
                >
                  {wm("buttons.viewSurface")}
                </button>
              )}
              <button
                type="button"
                data-testid="web-model-check-status"
                onClick={() => void checkBrowserStatus()}
                disabled={browserBusy || !normalized(actorId)}
                className={secondaryButtonClass("sm")}
              >
                {wm("buttons.checkStatus")}
              </button>
            </div>
          </div>
          <dl className="mt-4 grid gap-3 sm:grid-cols-3">
            <div>
              <dt className="text-xs text-[var(--color-text-muted)]">{wm("browser.stateLabel")}</dt>
              <dd className="mt-1 text-sm font-semibold text-[var(--color-text-primary)]">
                {normalized(actorId) ? browserStatusLabel : wm("browser.noActor")}
              </dd>
            </div>
            <div className="min-w-0">
              <dt className="text-xs text-[var(--color-text-muted)]">
                {wm("browser.currentChat")}
              </dt>
              <dd className="mt-1 text-sm text-[var(--color-text-secondary)]">
                {currentBrowserConversationUrl
                  ? shortConversationLabel(currentBrowserConversationUrl)
                  : wm("browser.currentChatNone")}
              </dd>
            </div>
            <div>
              <dt className="text-xs text-[var(--color-text-muted)]">{wm("browser.viewNote")}</dt>
              <dd className="mt-1 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {wm("browser.viewOnlyNote")}
              </dd>
            </div>
          </dl>
          {selectedBrowserSession?.error ? (
            <div className="mt-3 text-xs leading-5 text-rose-600 dark:text-rose-300">
              {selectedBrowserSession.error}
            </div>
          ) : null}
          {showBrowserSurface && normalized(groupId) && normalized(actorId) ? (
            <div data-testid="web-model-browser-surface" className="mt-3">
              {browserActive ? (
                <ProjectedBrowserSurfacePanel
                  isDark={isDark}
                  refreshNonce={0}
                  sessionIdentity={`${normalized(groupId)}:${normalized(actorId)}`}
                  defaultViewerMode="page"
                  chromeMode="embedded"
                  viewportClassName="h-[68vh] min-h-[460px] max-h-[780px]"
                  loadSession={loadSurfaceSession}
                  webSocketUrl={api.getWebModelBrowserSurfaceWebSocketUrl(groupId, actorId)}
                  fallbackUrl="https://chatgpt.com/"
                  labels={{
                    starting: wm("browserSurface.starting"),
                    waiting: wm("browserSurface.waiting"),
                    ready: wm("browserSurface.ready"),
                    failed: wm("browserSurface.failed"),
                    closed: wm("browserSurface.closed"),
                    reconnecting: wm("browserSurface.reconnecting"),
                    reconnect: wm("browserSurface.reconnect"),
                    frameAlt: wm("browserSurface.frameAlt"),
                  }}
                />
              ) : (
                <p
                  role="status"
                  className="rounded-lg border border-[var(--glass-border-subtle)] p-4 text-sm text-[var(--color-text-secondary)]"
                >
                  {wm("browser.previewClosed")}
                </p>
              )}
            </div>
          ) : null}
        </section>

        <section
          data-testid="web-model-groups"
          className={settingsWorkspacePanelClass(isDark)}
          aria-labelledby="web-model-groups-title"
        >
          <h4
            id="web-model-groups-title"
            className="text-sm font-semibold text-[var(--color-text-primary)]"
          >
            {wm("groups.title")}
          </h4>
          <p className="mt-1 text-xs leading-5 text-[var(--color-text-tertiary)]">
            {wm("groups.description")}
          </p>
          <div className="mt-4 grid gap-4 lg:grid-cols-[minmax(14rem,0.7fr)_minmax(0,1.3fr)]">
            <div role="group" aria-label={wm("groups.title")} data-testid="web-model-group-list">
              {groups.length ? (
                groups.map((group) => {
                  const rowId = normalized(group.group_id);
                  const selected = rowId === normalized(groupId);
                  return (
                    <button
                      key={rowId}
                      type="button"
                      data-testid={`web-model-group-${rowId}`}
                      aria-pressed={selected}
                      onClick={() => selectGroup(rowId)}
                      className={`mb-2 w-full rounded-xl border px-3 py-3 text-left transition-colors ${
                        selected
                          ? "border-[var(--color-text-primary)] bg-[var(--glass-tab-bg-hover)]"
                          : "border-[var(--glass-border-subtle)] bg-[var(--glass-panel-bg)] hover:bg-[var(--glass-tab-bg-hover)]"
                      }`}
                    >
                      <div className="flex items-start justify-between gap-3">
                        <span className="min-w-0 truncate text-sm font-semibold text-[var(--color-text-primary)]">
                          {normalized(group.title) || rowId}
                        </span>
                        <span className="shrink-0 text-xs text-[var(--color-text-secondary)]">
                          {groupRowStatus(rowId)}
                        </span>
                      </div>
                    </button>
                  );
                })
              ) : (
                <div className="rounded-xl border border-[var(--glass-border-subtle)] p-4 text-sm text-[var(--color-text-tertiary)]">
                  {wm("groups.empty")}
                </div>
              )}
              {onCreateGroup ? (
                <button
                  type="button"
                  data-testid="web-model-new-group"
                  onClick={() => onCreateGroup(normalized(groupId))}
                  className="flex w-full items-center gap-2 rounded-lg border border-dashed border-[var(--glass-border-subtle)] px-3 py-2 text-left text-sm font-medium text-[var(--color-text-secondary)] transition-colors hover:bg-[var(--glass-tab-bg-hover)] hover:text-[var(--color-text-primary)]"
                >
                  <PlusIcon size={15} aria-hidden="true" />
                  <span>{wm("buttons.newGroup")}</span>
                </button>
              ) : null}
            </div>

            <article
              data-testid="web-model-selected-group"
              className="min-w-0"
              aria-busy={actorsLoading}
            >
              <div className="flex flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
                <h5 className="truncate text-base font-semibold text-[var(--color-text-primary)]">
                  {normalized(
                    groups.find((group) => normalized(group.group_id) === groupId)?.title,
                  ) ||
                    groupId ||
                    wm("groups.empty")}
                </h5>
                <span
                  data-testid="web-model-group-row-status"
                  className="inline-flex shrink-0 rounded-full border border-[var(--glass-border-subtle)] px-2.5 py-1 text-xs font-semibold text-[var(--color-text-secondary)]"
                >
                  {selectedWebActor
                    ? groupRowStatus(normalized(groupId))
                    : wm("groups.notConnected")}
                </span>
              </div>

              <dl className="mt-4 grid gap-3 sm:grid-cols-2 2xl:grid-cols-3">
                <div className="min-w-0">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.foremanRole")}
                  </dt>
                  <dd className="mt-1 text-sm text-[var(--color-text-primary)]">
                    {actorsLoading
                      ? wm("common.loading")
                      : foremanActor
                        ? `${normalized(foremanActor.title || foremanActor.id)} · ${
                            foremanActor.running
                              ? wm("groupDetail.running")
                              : wm("groupDetail.stopped")
                          }`
                        : wm("groupDetail.noForeman")}
                  </dd>
                </div>
                <div className="min-w-0">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.runtime")}
                  </dt>
                  <dd className="mt-1 text-sm text-[var(--color-text-primary)]">
                    {foremanActor
                      ? webForeman
                        ? wm("groupDetail.runtimeWeb")
                        : wm("groupDetail.runtimeLocal")
                      : wm("groupDetail.runtimeNone")}
                  </dd>
                </div>
                <div className="min-w-0">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.members")}
                  </dt>
                  <dd className="mt-1 text-sm text-[var(--color-text-primary)]">
                    {wm("groupDetail.membersUnit", {
                      count: actors.filter(
                        (actor) => !actor.internal_kind && !isForemanActor(actor),
                      ).length,
                    })}
                  </dd>
                </div>
                <div className="min-w-0">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.queue")}
                  </dt>
                  <dd className="mt-1 text-sm text-[var(--color-text-primary)]">{queuedCount}</dd>
                </div>
                <div className="min-w-0">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.lastDelivery")}
                  </dt>
                  <dd className="mt-1 text-sm text-[var(--color-text-secondary)]">
                    {lastDeliveryText || wm("groupDetail.lastDeliveryNone")}
                  </dd>
                </div>
              </dl>

              <dl className="mt-5 space-y-4 border-t border-[var(--glass-border-subtle)] pt-4">
                <div data-testid="web-model-identity-status">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.chatToGroupTitle")}
                  </dt>
                  <dd className="mt-1">
                    <span
                      className={`inline-flex rounded-full border px-2 py-0.5 text-xs font-semibold ${setupPillClass(
                        selectedSessionBound ? "ready" : "needs",
                      )}`}
                    >
                      {selectedSessionBound
                        ? wm("groupDetail.identityBound")
                        : wm("groupDetail.identityNotBound")}
                    </span>
                    <span className="mt-1 block text-xs leading-5 text-[var(--color-text-tertiary)]">
                      {wm("groupDetail.identityNote")}
                    </span>
                  </dd>
                </div>
                <div data-testid="web-model-return-status">
                  <dt className="text-xs text-[var(--color-text-muted)]">
                    {wm("groupDetail.groupToChatTitle")}
                  </dt>
                  <dd className="mt-1">
                    <span
                      className={`inline-flex rounded-full border px-2 py-0.5 text-xs font-semibold ${setupPillClass(
                        returnBound || returnNewChat ? "ready" : "needs",
                      )}`}
                    >
                      {wm(returnStatusKey)}
                      {returnBound && boundConversationUrl
                        ? ` · ${shortConversationLabel(boundConversationUrl)}`
                        : ""}
                    </span>
                    <span className="mt-1 block text-xs leading-5 text-[var(--color-text-tertiary)]">
                      {wm("groupDetail.returnNote")}
                    </span>
                    {deliveryTargetSavedAt ? (
                      <span className="mt-1 block text-xs leading-5 text-[var(--color-text-tertiary)]">
                        {wm("groupDetail.savedAt", { time: formatTime(deliveryTargetSavedAt) })}
                      </span>
                    ) : null}
                  </dd>
                </div>
              </dl>

              {nextActionText ? (
                <p
                  data-testid="web-model-next-action"
                  role="status"
                  className="mt-3 text-xs leading-5 text-[var(--color-text-secondary)]"
                >
                  {nextActionText}
                </p>
              ) : null}

              <div className="mt-3 text-xs leading-5 text-[var(--color-text-tertiary)]">
                {selectedWebActor
                  ? `${wm("groupDetail.memberLabel")}: ${normalized(
                      selectedWebActor.title || selectedWebActor.id,
                    )} · ${
                      selectedWebActor.running
                        ? wm("groupDetail.running")
                        : wm("groupDetail.stopped")
                    }`
                  : wm("browser.noActor")}
              </div>
              {extraChatGptActors.length ? (
                <div className="mt-2 text-xs leading-5 text-amber-700 dark:text-amber-300">
                  {wm("groupDetail.multipleWarning", { count: webModelActors.length })}
                </div>
              ) : null}
              {foremanActor && !webForeman ? (
                <div className="mt-2 text-xs leading-5 text-[var(--color-text-tertiary)]">
                  {wm("groupDetail.localForemanNote")}
                </div>
              ) : null}

              <div className="mt-4 flex flex-wrap items-center gap-2 border-t border-[var(--glass-border-subtle)] pt-4">
                {onCreateActor ? (
                  foremanActor ? (
                    <Popover open={foremanMenuOpen} onOpenChange={setForemanMenuOpen}>
                      <PopoverTrigger asChild>
                        <button
                          type="button"
                          data-testid="web-model-change-foreman"
                          disabled={actorsLoading}
                          aria-haspopup="menu"
                          className={`${secondaryButtonClass("sm")} inline-flex items-center gap-1.5`}
                        >
                          <span>{wm("buttons.changeForeman")}</span>
                          <ChevronDownIcon size={14} aria-hidden="true" />
                        </button>
                      </PopoverTrigger>
                      <PopoverContent align="start" sideOffset={6} className="w-60 p-1">
                        <div role="menu" aria-label={wm("buttons.changeForeman")}>
                          <button
                            type="button"
                            role="menuitemradio"
                            aria-checked={Boolean(webForeman)}
                            data-testid="web-model-change-foreman-web"
                            onClick={() => {
                              setForemanMenuOpen(false);
                              if (webForeman) {
                                setEditorOpen(true);
                                return;
                              }
                              if (foremanActor) {
                                onEditActor?.(
                                  normalized(groupId),
                                  normalized(foremanActor.id),
                                  "web_model",
                                );
                              }
                            }}
                            className="flex min-h-[42px] w-full items-center justify-between gap-3 rounded-lg px-3 py-2 text-left text-sm text-[var(--color-text-primary)] transition-colors hover:bg-[var(--glass-tab-bg-hover)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[rgb(143,163,187)]/45"
                          >
                            <span>{wm("buttons.webModelOption")}</span>
                            {webForeman ? <CheckIcon size={15} aria-hidden="true" /> : null}
                          </button>
                          <button
                            type="button"
                            role="menuitemradio"
                            aria-checked={Boolean(foremanActor && !webForeman)}
                            data-testid="web-model-change-foreman-local"
                            onClick={() => {
                              setForemanMenuOpen(false);
                              if (!foremanActor) return;
                              onEditActor?.(
                                normalized(groupId),
                                normalized(foremanActor.id),
                                "local",
                              );
                            }}
                            className="flex min-h-[42px] w-full items-center justify-between gap-3 rounded-lg px-3 py-2 text-left text-sm text-[var(--color-text-primary)] transition-colors hover:bg-[var(--glass-tab-bg-hover)] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[rgb(143,163,187)]/45"
                          >
                            <span>{wm("buttons.localModelOption")}</span>
                            {foremanActor && !webForeman ? (
                              <CheckIcon size={15} aria-hidden="true" />
                            ) : null}
                          </button>
                        </div>
                      </PopoverContent>
                    </Popover>
                  ) : (
                    <button
                      type="button"
                      data-testid="web-model-add-foreman"
                      disabled={actorsLoading || !groupId}
                      onClick={() => onCreateActor(normalized(groupId), { role: "foreman" })}
                      className={primaryButtonClass(false)}
                    >
                      {wm("buttons.addForeman")}
                    </button>
                  )
                ) : null}
                {onCreateActor && foremanActor ? (
                  <button
                    type="button"
                    data-testid="web-model-add-member"
                    onClick={() => onCreateActor(normalized(groupId), { role: "peer" })}
                    className={secondaryButtonClass("sm")}
                  >
                    {wm("buttons.addMember")}
                  </button>
                ) : null}
                {selectedWebActor ? (
                  <button
                    type="button"
                    data-testid="web-model-copy-instructions"
                    onClick={() => void copyConnectionInstructions()}
                    disabled={bindingBusy || createBusy || !normalized(actorId)}
                    className={primaryButtonClass(bindingBusy || createBusy)}
                  >
                    {wm("buttons.copyInstructions")}
                  </button>
                ) : null}
                {selectedSessionBound && selectedConnector ? (
                  <button
                    type="button"
                    data-testid="web-model-disconnect"
                    onClick={() => void revokeConnector(selectedConnector.connector_id)}
                    disabled={revokeBusyId === selectedConnector.connector_id || actorsLoading}
                    className={dangerButtonClass("sm")}
                  >
                    {wm("buttons.disconnectChat")}
                  </button>
                ) : null}
                {onOpenGuidance ? (
                  <button
                    type="button"
                    data-testid="web-model-guidance"
                    disabled={actorsLoading || !groupId}
                    onClick={() => onOpenGuidance(normalized(groupId))}
                    className="min-h-[44px] px-1 text-sm font-semibold text-[var(--color-text-secondary)] underline decoration-[var(--glass-border-subtle)] underline-offset-4 hover:text-[var(--color-text-primary)]"
                  >
                    {wm("buttons.guidance")}
                  </button>
                ) : null}
              </div>
              {foremanActor && !webForeman ? (
                <p className="mt-2 text-xs leading-5 text-[var(--color-text-tertiary)]">
                  {wm("groupDetail.changeForemanHint")}
                </p>
              ) : null}

              {editorOpen && selectedWebActor ? (
                <div
                  data-testid="web-model-target-editor"
                  className="mt-4 rounded-xl border border-[var(--glass-border-subtle)] bg-[var(--glass-panel-bg)] p-4"
                >
                  <div className="text-sm font-semibold text-[var(--color-text-primary)]">
                    {wm("editor.title")}
                  </div>
                  <p className="mt-1 text-xs leading-5 text-[var(--color-text-tertiary)]">
                    {wm("editor.description")}
                  </p>
                  <fieldset className="mt-3 space-y-2">
                    <legend className="sr-only">{wm("editor.title")}</legend>
                    <label className={targetRadioClass("current")}>
                      <input
                        type="radio"
                        name="web-model-target"
                        value="current"
                        data-testid="web-model-editor-current"
                        checked={targetChoice === "current"}
                        onChange={() => chooseTargetChoice("current", "existing")}
                        className="mt-0.5 h-4 w-4 shrink-0 accent-[rgb(35,36,37)] dark:accent-white"
                      />
                      <span className="min-w-0">
                        <span className="block font-semibold">{wm("editor.choiceCurrent")}</span>
                        <span className="mt-0.5 block text-xs leading-5 text-[var(--color-text-tertiary)]">
                          {wm("editor.choiceCurrentDetail")}
                        </span>
                        {targetChoice === "current" ? (
                          <span className="mt-2 flex flex-wrap items-center gap-2">
                            <button
                              type="button"
                              data-testid="web-model-read-current-chat"
                              onClick={() => void readCurrentChat()}
                              disabled={browserBusy}
                              className={secondaryButtonClass("sm")}
                            >
                              {wm("buttons.readCurrentChat")}
                            </button>
                            <span className="text-xs text-[var(--color-text-tertiary)]">
                              {currentBrowserConversationUrl
                                ? wm("editor.currentChatAvailable", {
                                    target: shortConversationLabel(currentBrowserConversationUrl),
                                  })
                                : wm("editor.currentChatUnavailable")}
                            </span>
                          </span>
                        ) : null}
                      </span>
                    </label>

                    <label className={targetRadioClass("pasted")}>
                      <input
                        type="radio"
                        name="web-model-target"
                        value="pasted"
                        data-testid="web-model-editor-pasted"
                        checked={targetChoice === "pasted"}
                        onChange={() => chooseTargetChoice("pasted", "existing")}
                        className="mt-0.5 h-4 w-4 shrink-0 accent-[rgb(35,36,37)] dark:accent-white"
                      />
                      <span className="min-w-0">
                        <span className="block font-semibold">{wm("editor.choicePasted")}</span>
                        <span className="mt-0.5 block text-xs leading-5 text-[var(--color-text-tertiary)]">
                          {wm("editor.choicePastedDetail")}
                        </span>
                        {targetChoice === "pasted" ? (
                          <span className="mt-2 block">
                            <span className={labelClass(isDark)}>{wm("editor.urlLabel")}</span>
                            <input
                              data-testid="web-model-target-url"
                              value={conversationUrlDraft}
                              onChange={(event) => {
                                setTargetChoice("pasted");
                                setTargetChoiceTouched(true);
                                setTargetDraftMode("existing");
                                setConversationUrlDraft(event.target.value);
                                setTargetDraftTouched(true);
                              }}
                              placeholder="https://chatgpt.com/c/..."
                              className={inputClass(isDark)}
                            />
                          </span>
                        ) : null}
                      </span>
                    </label>

                    <label className={targetRadioClass("new")}>
                      <input
                        type="radio"
                        name="web-model-target"
                        value="new"
                        data-testid="web-model-editor-new"
                        checked={targetChoice === "new"}
                        onChange={() => chooseTargetChoice("new", "new")}
                        className="mt-0.5 h-4 w-4 shrink-0 accent-[rgb(35,36,37)] dark:accent-white"
                      />
                      <span className="min-w-0">
                        <span className="block font-semibold">{wm("editor.choiceNew")}</span>
                        <span className="mt-0.5 block text-xs leading-5 text-[var(--color-text-tertiary)]">
                          {wm("editor.choiceNewDetail")}
                        </span>
                      </span>
                    </label>
                  </fieldset>
                  {targetDraftError ? (
                    <div className="mt-2 text-xs leading-5 text-amber-700 dark:text-amber-300">
                      {targetDraftError}
                    </div>
                  ) : null}
                  <div className="mt-3 flex flex-wrap items-center justify-between gap-2 border-t border-[var(--glass-border-subtle)] pt-3">
                    <span className="text-xs leading-5 text-[var(--color-text-secondary)]">
                      {targetDraftDirty ? wm("editor.unsaved") : wm("editor.noUnsaved")}
                    </span>
                    <span className="flex flex-wrap gap-2">
                      <button
                        type="button"
                        data-testid="web-model-cancel-target-edit"
                        onClick={cancelTargetEdit}
                        disabled={!targetDraftDirty}
                        className={secondaryButtonClass("sm")}
                      >
                        {wm("buttons.cancelTargetEdit")}
                      </button>
                      <button
                        type="button"
                        data-testid="web-model-save-target"
                        onClick={() => void saveDeliveryTarget()}
                        disabled={targetSaveDisabled}
                        className={
                          targetDraftDirty
                            ? primaryButtonClass(browserBusy)
                            : secondaryButtonClass("sm")
                        }
                      >
                        {wm("buttons.saveTarget")}
                      </button>
                    </span>
                  </div>
                </div>
              ) : null}

              <details
                data-testid="web-model-advanced"
                className="mt-4 text-xs leading-5 text-[var(--color-text-secondary)]"
              >
                <summary className="cursor-pointer font-semibold text-[var(--color-text-primary)]">
                  {wm("advanced.summary")}
                </summary>
                <dl className="mt-2 grid gap-x-4 gap-y-1 sm:grid-cols-[11rem_minmax(0,1fr)]">
                  <dt>{wm("advanced.browser")}</dt>
                  <dd>{browserStatusLabel}</dd>
                  <dt>{wm("advanced.currentTarget")}</dt>
                  <dd className="break-all">
                    {boundConversationUrl
                      ? shortConversationLabel(boundConversationUrl)
                      : pendingNewChatBind
                        ? wm("groupDetail.returnNewChat")
                        : wm("common.none")}
                  </dd>
                  <dt>{wm("advanced.mcpApp")}</dt>
                  <dd>
                    {selectedSessionBound
                      ? wm("groupDetail.identityBound")
                      : !selectedConnector
                        ? wm("reference.mcpNotCreated")
                        : !selectedMcpUrl
                          ? wm("reference.mcpNeedsRotation")
                          : mcpLastCallFailed
                            ? wm("reference.mcpLastCallFailed")
                            : chatGptSeen
                              ? wm("reference.mcpSeenAt", {
                                  time: formatTime(selectedConnector?.last_activity_at),
                                })
                              : wm("reference.mcpWaitingFirstCall")}
                  </dd>
                  {selectedConnector?.connector_id ? (
                    <>
                      <dt>{wm("advanced.mcpUrlId")}</dt>
                      <dd className="break-all font-mono">{selectedConnector.connector_id}</dd>
                    </>
                  ) : null}
                  {selectedConnector ? (
                    <>
                      <dt>{wm("advanced.remote")}</dt>
                      <dd>{connectorActivityLabel(selectedConnector, wm)}</dd>
                    </>
                  ) : null}
                  {selectedConnector?.last_error ? (
                    <>
                      <dt>{wm("advanced.lastMcpError")}</dt>
                      <dd className="break-all text-rose-600 dark:text-rose-300">
                        {selectedConnector.last_error}
                      </dd>
                    </>
                  ) : null}
                  <dt>{wm("advanced.lastDelivery")}</dt>
                  <dd className="break-all">
                    {lastDeliveryText || wm("common.none")}
                    {selectedBrowserSession?.last_delivery_status
                      ? ` · ${wm(`deliveryState.${selectedBrowserSession.last_delivery_status}`, { defaultValue: selectedBrowserSession.last_delivery_status })}`
                      : ""}
                  </dd>
                  {selectedBrowserSession?.last_error ? (
                    <>
                      <dt>{wm("advanced.lastError")}</dt>
                      <dd className="break-all text-rose-600 dark:text-rose-300">
                        {selectedBrowserSession.last_error}
                      </dd>
                    </>
                  ) : null}
                  {!boundConversationUrl && pendingNewChatBind ? (
                    <>
                      <dt>{wm("advanced.pendingNewChat")}</dt>
                      <dd className="break-all font-mono">
                        {normalized(selectedBrowserSession?.pending_new_chat_url) ||
                          "https://chatgpt.com/"}
                      </dd>
                    </>
                  ) : null}
                  {selectedBrowserSession?.profile_dir ? (
                    <>
                      <dt>{wm("advanced.profileDir")}</dt>
                      <dd className="break-all font-mono">{selectedBrowserSession.profile_dir}</dd>
                    </>
                  ) : null}
                  {selectedBrowserSession?.visibility ? (
                    <>
                      <dt>{wm("advanced.mode")}</dt>
                      <dd>
                        {wm(`browserMode.${selectedBrowserSession.visibility}`, {
                          defaultValue: selectedBrowserSession.visibility,
                        })}
                      </dd>
                    </>
                  ) : null}
                  {selectedHealth?.delivery?.mode ? (
                    <>
                      <dt>{wm("advanced.mode")}</dt>
                      <dd>
                        {wm(`deliveryMode.${selectedHealth.delivery.mode}`, {
                          defaultValue: selectedHealth.delivery.mode,
                        })}
                      </dd>
                    </>
                  ) : null}
                </dl>
                <p className="mt-3 text-xs leading-5 text-[var(--color-text-tertiary)]">
                  {wm("advanced.callbackNote")}
                </p>
                <div className="mt-3 flex flex-wrap gap-2">
                  <button
                    type="button"
                    onClick={() => void createConnectorManual()}
                    disabled={createBusy || !normalized(actorId)}
                    className={secondaryButtonClass("sm")}
                  >
                    {selectedConnector ? wm("buttons.rotateMcpUrl") : wm("buttons.createMcpUrl")}
                  </button>
                  {selectedConnector ? (
                    <button
                      type="button"
                      onClick={() => void revokeConnector(selectedConnector.connector_id)}
                      disabled={revokeBusyId === normalized(selectedConnector.connector_id)}
                      className={dangerButtonClass("sm")}
                    >
                      {wm("buttons.revokeMcpUrl")}
                    </button>
                  ) : null}
                  <button
                    type="button"
                    onClick={() => void closeBrowser()}
                    disabled={browserBusy || !browserActive}
                    className={secondaryButtonClass("sm")}
                  >
                    {wm("buttons.closeBrowser")}
                  </button>
                </div>
                <p className="mt-2 text-xs leading-5 text-amber-700 dark:text-amber-300">
                  {wm("advanced.closeBrowserWarning")}
                </p>
              </details>
            </article>
          </div>
        </section>
      </div>

      <div className="sr-only" aria-live="polite">
        {notice}
      </div>
    </div>
  );
}
