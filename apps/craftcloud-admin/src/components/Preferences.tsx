import { useI18n } from "../i18n/index.tsx";
import { THEME_CHOICES } from "../preferences.ts";
import type { ThemeChoice } from "../preferences.ts";
import { NativeSelect } from "./ContextSelect.tsx";

/** Language and theme switches; both are remembered in localStorage only. */
export function Preferences({
  theme,
  onThemeChange,
}: {
  theme: ThemeChoice;
  onThemeChange: (choice: ThemeChoice) => void;
}) {
  const { t, language, languages, setLanguage } = useI18n();
  const themeLabel: Record<ThemeChoice, string> = {
    system: t("Match system"),
    light: t("Light"),
    dark: t("Dark"),
  };
  return (
    <div className="shell__prefs">
      <NativeSelect
        label={t("Language")}
        value={language}
        options={languages.map((entry) => ({ id: entry.code, label: entry.name }))}
        onChange={setLanguage}
      />
      <NativeSelect
        label={t("Theme")}
        value={theme}
        options={THEME_CHOICES.map((choice) => ({ id: choice, label: themeLabel[choice] }))}
        onChange={(value) => {
          const choice = THEME_CHOICES.find((candidate) => candidate === value);
          if (choice !== undefined) {
            onThemeChange(choice);
          }
        }}
      />
    </div>
  );
}
