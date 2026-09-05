import { useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Actor, SupportedRuntime } from "../../types";
import { useFormStore, useGroupStore, useModalStore, useUIStore } from "../../stores";
import * as api from "../../services/api";
import { Popover, PopoverContent, PopoverTrigger } from "../ui/popover";
import { T05ChangeMark } from "../T05ChangeMark";

type Props = {
  groupId: string;
  actors: Actor[];
  readOnly: boolean;
  onOpenActor: (actorId: string) => void;
  onEditActor: (actor: Actor) => void;
};

/** Group-scoped shortcut only. Configuration/lifecycle stay in the native editors. */
export function GroupMembersMenu({ groupId, actors, readOnly, onOpenActor, onEditActor }: Props) {
  const { t } = useTranslation("actors");
  const [open, setOpen] = useState(false);
  const [choosing, setChoosing] = useState(false);
  const selecting = useRef(false);
  const members = actors.filter((actor) => !actor.internal_kind);
  const foreman = members.find((actor) => actor.role === "foreman");
  const changeForeman = async (runtime: "web_model" | "local") => {
    if (!foreman || readOnly || selecting.current) return;
    if ((runtime === "web_model") === (foreman.runtime === "web_model")) {
      setOpen(false);
      if (runtime === "web_model")
        useModalStore.getState().openSettingsTarget({ scope: "global", tab: "webModels" });
      else onEditActor(foreman);
      return;
    }
    selecting.current = true;
    setChoosing(true);
    try {
      let selected: SupportedRuntime = "web_model";
      let command = "";
      if (runtime === "local") {
        const response = await api.fetchRuntimes();
        if (!response.ok) throw new Error(response.error.message);
        const local =
          response.result.runtimes.find((item) => item.available && item.name === "opencode") ||
          response.result.runtimes.find(
            (item) => item.available && item.name !== "web_model" && item.name !== "custom",
          );
        if (!local) throw new Error(t("t05Members.noLocalRuntime"));
        selected = local.name as SupportedRuntime;
        command = local.recommended_command || "";
        useGroupStore.getState().setRuntimes(response.result.runtimes);
      }
      if (useGroupStore.getState().selectedGroupId !== groupId) return;
      const current = useGroupStore.getState().actors.find((actor) => actor.id === foreman.id);
      if (!current || current.role !== "foreman") return;
      onEditActor(current);
      useFormStore.getState().setEditActorRuntime(selected);
      useFormStore.getState().setEditActorCommand(command);
      setOpen(false);
    } catch (error) {
      if (useGroupStore.getState().selectedGroupId === groupId)
        useUIStore.getState().showError(String(error));
    } finally {
      selecting.current = false;
      setChoosing(false);
    }
  };
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          data-t05-change="members-entry"
          className="inline-flex shrink-0 items-center rounded-lg border border-[var(--glass-border-subtle)] px-2 py-1.5 text-xs text-[var(--color-text-primary)]"
        >
          {t("t05Members.entry", { count: members.length })}
          <T05ChangeMark />
        </button>
      </PopoverTrigger>
      <PopoverContent
        align="start"
        className="w-[min(22rem,calc(100vw-2rem))] bg-[var(--color-bg-secondary)] p-3"
        data-t05-change="members-menu"
      >
        <div className="mb-2 text-sm font-semibold">
          {t("t05Members.title")}
          <T05ChangeMark />
        </div>
        <p className="mb-2 text-xs text-[var(--color-text-tertiary)]">
          {t("t05Members.nativeEditors")}
        </p>
        <div className="max-h-[45vh] overflow-y-auto">
          {members.map((actor) => (
            <button
              key={actor.id}
              type="button"
              data-t05-change="member-details"
              data-actor-id={actor.id}
              onClick={() => {
                setOpen(false);
                onOpenActor(actor.id);
              }}
              className="flex w-full items-center justify-between gap-2 rounded-lg px-2 py-2 text-left hover:bg-[var(--glass-tab-bg-hover)]"
            >
              <span className="min-w-0">
                <span className="block truncate text-sm">{actor.title || actor.id}</span>
                <span className="text-xs text-[var(--color-text-tertiary)]">
                  {actor.role === "foreman" ? t("t05Members.foreman") : t("t05Members.peer")} ·{" "}
                  {actor.runtime === "web_model"
                    ? t("t05Members.web_model")
                    : actor.runtime === "custom"
                      ? t("custom")
                      : actor.runtime}
                </span>
              </span>
              <T05ChangeMark />
            </button>
          ))}
          {!members.length && <p className="py-2 text-sm">{t("t05Members.empty")}</p>}
        </div>
        {!readOnly && (
          <div className="mt-2 space-y-2 border-t border-[var(--glass-border-subtle)] pt-2">
            {foreman && (
              <details>
                <summary className="cursor-pointer text-sm" data-t05-change="change-foreman">
                  {t("t05Members.changeForeman")}
                  <T05ChangeMark />
                </summary>
                <div className="mt-2 flex gap-2">
                  {(["web_model", "local"] as const).map((kind) => (
                    <button
                      key={kind}
                      type="button"
                      data-t05-change={`foreman-${kind}`}
                      disabled={choosing}
                      onClick={() => void changeForeman(kind)}
                      className="rounded-lg border border-[var(--glass-border-subtle)] px-2 py-1 text-sm disabled:opacity-50"
                    >
                      {t(`t05Members.${kind}`)}
                      <T05ChangeMark />
                    </button>
                  ))}
                </div>
              </details>
            )}
            <button
              type="button"
              data-t05-change="add-member"
              className="inline-flex items-center text-sm"
              onClick={() => {
                useFormStore.getState().setNewActorRole(foreman ? "peer" : "foreman");
                useModalStore.getState().openModal("addActor");
                setOpen(false);
              }}
            >
              {t("t05Members.add")}
              <T05ChangeMark />
            </button>
          </div>
        )}
      </PopoverContent>
    </Popover>
  );
}
