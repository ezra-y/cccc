import { useTranslation } from "react-i18next";

/** Temporary review annotation requested by the owner; does not change hit areas. */
export function T05ChangeMark() {
  const { t } = useTranslation("common");
  return (
    <span
      title={t("t05Change")}
      className="ml-1 inline-flex shrink-0 align-middle"
      data-t05-mark="true"
    >
      <svg width="15" height="15" viewBox="0 0 16 16" aria-hidden="true" focusable="false">
        <circle cx="8" cy="8" r="6" fill="none" stroke="#ef4444" strokeWidth="2" />
      </svg>
      <span className="sr-only">{t("t05Change")}</span>
    </span>
  );
}
