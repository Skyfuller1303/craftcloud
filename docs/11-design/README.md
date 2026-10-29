# Design der Admin-Oberfläche — Briefing

> **Für wen:** eine Coding-Agent-Instanz (z. B. Codex) oder eine Person, die die Gestaltung von `apps/craftcloud-admin` vollständig umsetzt. **Status:** verbindlich, Stand 2026-10-29. **Branch:** `feat/admin-design`. **Entscheidung:** ADR-0015 und Wunsch des Projektverantwortlichen: Orientierung am Craft-Launcher.

Dieser Ordner ist die einzige Quelle für das Aussehen der Admin-Oberfläche. Er beschreibt, **was** gebaut wird (Farben, Schrift, Abstände, Elemente, Ansichten), **wie** gearbeitet wird (Dateien, Befehle, Grenzen) und **wann** es fertig ist (Abnahme). Lies ihn vollständig, bevor du Code änderst.

## Ziel in einem Satz

Die funktional fertige, aber visuell rohe Admin-Oberfläche soll aussehen wie ein Geschwister des Craft-Launchers: ruhig, dicht, eckig, Haarlinien, Monospace-Großbuchstaben für Labels und Buttons, invertierte Primäraktionen, sparsamer Blau-Akzent — barrierefrei nach WCAG 2.2 AA, ohne ein einziges Craft-Asset.

## Lesereihenfolge

| Datei | Inhalt |
|---|---|
| [01-principles.md](01-principles.md) | Grundsätze, Charakter, Do/Don't |
| [02-brand-and-logos.md](02-brand-and-logos.md) | Name, Wortmarke, Tag, Favicon, verbotene Assets, Attribution |
| [03-colors.md](03-colors.md) | Farb-Tokens hell/dunkel, Regeln, Kontrastpaare |
| [04-typography.md](04-typography.md) | Schriftstapel, Skala, Großschreibung, Zahlen |
| [05-spacing-and-layout.md](05-spacing-and-layout.md) | Abstände, Maße, Shell, Navigation, Breakpoints |
| [06-components.md](06-components.md) | jedes Element mit Maßen und Zuständen |
| [07-icons.md](07-icons.md) | Icon-Komponente und vollständiger Satz |
| [08-screens.md](08-screens.md) | jede Ansicht im Detail |
| [09-motion-and-feedback.md](09-motion-and-feedback.md) | Bewegung, Toasts, Laden, Leer, Fehler |
| [10-accessibility.md](10-accessibility.md) | WCAG-Anforderungen und Testbarkeit |
| [11-content-and-i18n.md](11-content-and-i18n.md) | Texte, Übersetzungen, Schreibweisen |
| [12-launcher-reference.md](12-launcher-reference.md) | Messwerte aus dem Launcher-Code mit Fundstellen |
| [13-current-state.md](13-current-state.md) | Bestand, bekannte Inkonsistenzen, bisherige Teilumsetzung |
| [14-workflow-and-acceptance.md](14-workflow-and-acceptance.md) | Arbeitsweise, Dateien, Befehle, Definition of Done |

## Harte Grenzen (Kurzfassung)

1. **Keine Craft-Assets:** keine Logos, App-Icons, Schriften (Inter, Archivo, Geist Mono), Bilder oder Illustrationen der Craft-Apps oder von getartcraft.com. Nur Systemschriften und selbst gezeichnete, einfache SVG-Formen. Übernommene **Zahlenwerte** (Farben, Maße) sind erlaubt und werden in `ATTRIBUTION.md` vermerkt.
2. **Strikte CSP:** kein Inline-CSS, keine React-`style`-Props, kein `dangerouslySetInnerHTML`, keine externen Ressourcen. Alles aus `'self'`.
3. **Verhalten bleibt:** Texte, API-Aufrufe, Zustandslogik, Rollen und zugängliche Namen der Bedienelemente ändern sich nicht. Die bestehenden Tests sind die Absicherung.
4. **Keine neuen Abhängigkeiten:** weder npm noch Rust. `package.json`/`package-lock.json` bleiben unverändert.
5. **Barrierefreiheit geht vor Optik:** jede Regel aus [10-accessibility.md](10-accessibility.md) ist Abnahmekriterium.

## Umfang

Gestaltet wird alles unter `apps/craftcloud-admin/src` (Styles, Komponenten, Markup der Ansichten), `apps/craftcloud-admin/index.html` (nur Favicon-Link, CSP-Meta bleibt), `apps/craftcloud-admin/public/` (nur ein eigenes Favicon) und `ATTRIBUTION.md`. Nicht Teil: Server (Rust), API, neue Funktionen (Sortierung, Suche, Zeilenmenüs, neue Ansichten). Details in [14-workflow-and-acceptance.md](14-workflow-and-acceptance.md).
