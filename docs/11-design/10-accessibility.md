# 10 — Barrierefreiheit (WCAG 2.2 AA)

Jeder Punkt ist Abnahmekriterium. Die bestehenden Tests (vitest mit axe-core in EN und DE, Fokus-Reihenfolge, Tastaturabläufe) müssen weiter bestehen.

## Wahrnehmbar

- **Kontrast Text** ≥ 4,5:1, auch für 11-px-Labels und 10-px-Badges (sie gelten als normale Schrift). Die Paare in [03 §Kontrastpaare](03-colors.md#kontrastpaare-müssen-im-test-geprüft-werden) werden im Token-Test geprüft.
- **Kontrast Nicht-Text** ≥ 3:1: Rahmen von Feldern, Selects, Checkboxen, sekundären Buttons (`--control-border`), Fokusring, Fortschrittsfüllung gegen die Spur.
- **Nicht nur Farbe:** Status hat immer Text (Badge). Statusquadrate stehen neben Text. Pflicht-/Fehlerzustände haben Text.
- **Großschreibung per CSS**, Originaltext im DOM.
- **Responsiv:** 360 px ohne horizontales Seiten-Scrollen (1.4.10), 200 % Zoom, Textabstände nach 1.4.12 dürfen nichts abschneiden (keine festen Höhen für Textcontainer außer Bedienelementen mit `min-height`).
- **Bilder:** Favicon und Icons `aria-hidden`; keine Informationsbilder.

## Bedienbar

- **Tastatur:** alles erreichbar, sichtbarer Fokus (`:focus-visible`, 2 px `--accent-ink`, nie entfernt außer an `tabindex="-1"`-Überschriften), Tab-Reihenfolge = visuelle Reihenfolge (Dialog-Fuß: Abbrechen vor Bestätigen in DOM **und** Optik).
- **Fokus nicht verdeckt** (2.4.11): Sticky-Navigation und Toasts dürfen fokussierte Elemente nicht überdecken; Toasts bei Fokus in ihrer Nähe nicht über Bedienelemente legen (`scroll-padding-bottom` auf `html` passend zur Toast-Höhe, falls nötig).
- **Zielgröße** (2.5.8) ≥ 24 × 24 px; Standard 34 px, klein 28 px.
- **Ziehen** nicht nötig.
- **Label im Namen** (2.5.3): `aria-label` beginnt mit dem sichtbaren Text (bereits so, z. B. „In den Papierkorb: Brand kit“). Beibehalten.

## Verständlich

- Sprache am `<html lang>` wird bereits gesetzt; Großschreibung hängt davon ab.
- Fehlermeldungen am Feld, `aria-invalid`, `aria-describedby` (RAC macht das).
- Konsistente Navigation und Benennung (gleiche Rolle = gleiche Form).

## Robust

- Rollen der RAC-Komponenten nicht überschreiben.
- Live-Region: genau eine (`Announcer`), Toasts `aria-hidden`.
- Tabellen behalten `<table>`-Semantik auch in der Kartenansicht (nur CSS ändert die Darstellung; Kopf visuell versteckt, nicht entfernt).

## Bewegung

`prefers-reduced-motion` respektieren ([09](09-motion-and-feedback.md)).

## Prüfmethode

1. `npm test` (jsdom + axe, ohne Kontrast) — muss grün bleiben.
2. Token-Kontrasttest erweitert ([03](03-colors.md)).
3. Echter Browser (Edge/Chrome headless über DevTools, durch den Koordinator): axe mit Kontrast auf jeder Ansicht und jedem Dialog, hell und dunkel, EN und DE, 1280 px und 360 px.
