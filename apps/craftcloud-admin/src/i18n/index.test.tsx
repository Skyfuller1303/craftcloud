import { act, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { CATALOGS, I18nProvider, createTranslator, loadCatalogs, negotiateLanguage, useI18n } from "./index.tsx";

describe("loadCatalogs", () => {
  it("derives the language from the file name and ignores other files", () => {
    const catalogs = loadCatalogs({
      "../locales/en.tsv": "\tHello\tHello\n",
      "../locales/pt-BR.tsv": "\tHello\tOlá\n",
      "../locales/notes.txt": "\tHello\tnope\n",
    });
    expect([...catalogs.keys()].sort()).toEqual(["en", "pt-br"]);
  });

  it("treats an empty translation as untranslated", () => {
    const catalogs = loadCatalogs({ "x/de.tsv": "\tA\t\n\tB\tb\n" });
    const { t } = createTranslator(catalogs.get("de"));
    expect(t("A")).toBe("A");
    expect(t("B")).toBe("b");
  });
});

describe("translator", () => {
  it("falls back to the English source for unknown texts and formats placeholders", () => {
    const { t, tc } = createTranslator(loadCatalogs({ "de.tsv": "\tHi {n}\tHallo {n}\nmenu\tOpen\tÖffnen\n" }).get("de"));
    expect(t("Hi {n}", { n: 2 })).toBe("Hallo 2");
    expect(t("Unknown {n}", { n: 2 })).toBe("Unknown 2");
    expect(tc("menu", "Open")).toBe("Öffnen");
    expect(t("Open")).toBe("Open");
  });
});

describe("negotiateLanguage", () => {
  const available = ["en", "de"];
  it("prefers the first supported browser language, also by base language", () => {
    expect(negotiateLanguage(["fr-FR", "de-AT", "en"], available)).toBe("de");
    expect(negotiateLanguage(["EN-us"], available)).toBe("en");
  });
  it("falls back to English", () => {
    expect(negotiateLanguage(["fr", "ja"], available)).toBe("en");
    expect(negotiateLanguage([], available)).toBe("en");
  });
});

describe("bundled catalogues", () => {
  it("contain English and German", () => {
    expect([...CATALOGS.keys()].sort()).toEqual(["de", "en"]);
  });
});

function Probe() {
  const { t, language, setLanguage } = useI18n();
  return (
    <div>
      <p data-testid="text">{t("Log in")}</p>
      <p data-testid="lang">{language}</p>
      <button onClick={() => setLanguage("de")}>de</button>
      <button onClick={() => setLanguage("xx")}>xx</button>
    </div>
  );
}

describe("I18nProvider", () => {
  it("switches language, remembers it and sets the document language", () => {
    render(
      <I18nProvider initial="en">
        <Probe />
      </I18nProvider>,
    );
    expect(screen.getByTestId("text").textContent).toBe("Log in");
    act(() => screen.getByText("de").click());
    expect(screen.getByTestId("text").textContent).toBe("Anmelden");
    expect(document.documentElement.lang).toBe("de");
    expect(window.localStorage.getItem("craftcloud.admin.language")).toBe("de");
    act(() => screen.getByText("xx").click());
    expect(screen.getByTestId("lang").textContent).toBe("de");
  });

  it("survives unavailable storage", () => {
    const original = Storage.prototype.setItem;
    Storage.prototype.setItem = () => {
      throw new Error("blocked");
    };
    try {
      render(
        <I18nProvider initial="en">
          <Probe />
        </I18nProvider>,
      );
      act(() => screen.getByText("de").click());
      expect(screen.getByTestId("text").textContent).toBe("Anmelden");
    } finally {
      Storage.prototype.setItem = original;
    }
  });
});
