# 14 — Arbeitsweise und Abnahme

## Arbeitsort

- Worktree `D:\ArtCraft\CraftCloud\wt-design`, Branch `feat/admin-design`. Kein anderer Checkout, kein neuer Clone.
- Nicht committen, pushen oder mergen; das macht der Koordinator nach Abnahme.

## Erlaubte Dateien

- `apps/craftcloud-admin/src/**` (Styles, Komponenten, Ansichten, Tests).
- `apps/craftcloud-admin/index.html` nur für den Favicon-Link.
- `apps/craftcloud-admin/public/favicon.svg` (neu).
- `apps/craftcloud-admin/ATTRIBUTION.md`.
- `src/locales/en.tsv` und `de.tsv` nur nach [11](11-content-and-i18n.md).

Nicht ändern: `package.json`, `package-lock.json`, `.npmrc`, `vite.config.*`, Rust-Code, `docs/` (Abweichungen im Ergebnisbericht nennen, nicht die Doku umschreiben).

## Dateistruktur

`src/styles/main.scss` wird zum Einstieg, der Partials einbindet (`@use`):

| Partial | Inhalt |
|---|---|
| `_tokens.scss` | alle Tokens, beide Themes ([03](03-colors.md), [05](05-spacing-and-layout.md)) |
| `_base.scss` | Reset, Typografie, Fokus, `visually-hidden`, Reduced Motion |
| `_layout.scss` | Frame, Shell, Kopfleiste, Navigation, Seitenkopf, Abschnitte, Toolbar |
| `_controls.scss` | Buttons, Felder, Select, Checkbox, Segmente |
| `_data.scss` | Tabellen (inkl. Kartenansicht), Badges, Tags, Fakten |
| `_overlays.scss` | Dialog, Popover, Toast |
| `_feedback.scss` | Banner, Leer/Laden, Fortschritt, Schritte, Kopierfeld |

Klassen nach BEM wie bisher. Neue Komponenten: `Icon.tsx`, `Badge.tsx`, `Banner.tsx` (ersetzt `Alert`/`ErrorNotice`-Darstellung bei gleicher API), `EmptyState.tsx`, `Toaster.tsx`. Bestehende Komponenten-APIs bleiben kompatibel.

## Befehle

Node liegt lokal, nicht im PATH. Aus `apps/craftcloud-admin`:

```powershell
$env:PATH = "D:\ArtCraft\.tools\node;$env:PATH"
$env:npm_config_cache = "D:\ArtCraft\.tools\node-cache"
npm run typecheck
npm test
npm run i18n:check
npm run build
```

Kein `npm install`/`npm ci` mit neuen Paketen; `node_modules` ist vorhanden. Falls `rtk` im Sandbox nicht startet („Zugriff verweigert“): Befehle ohne `rtk` ausführen und das im Bericht nennen; nicht an Berechtigungen drehen.

Rust-Prüfung (`cargo xtask ci`) und Browser-Screenshots macht der Koordinator.

## Vorgehen

1. [README](README.md) und 01–13 lesen.
2. Tokens und Partials zuerst (Schritt bauen, Tests grün).
3. Basiskomponenten (Icon, Badge, Banner, EmptyState, Buttons, Felder, Select, Tabellen, Dialog).
4. Shell und Auth-Karten.
5. Ansichten nach [08](08-screens.md).
6. Toasts, Reduced Motion, Kartenansicht 360 px.
7. Favicon, Wortmarke, `ATTRIBUTION.md`.
8. Nach jedem Schritt `typecheck` + `test`. Nie mit roten Tests weitermachen.

## Abnahme (Definition of Done)

1. `npm run typecheck`, `npm test` (inkl. axe EN/DE), `npm run i18n:check`, `npm run build` grün — tatsächlich ausgeführt, Exitcodes im Bericht.
2. Keine `style`-Props, kein `dangerouslySetInnerHTML`, kein `style=` im Build außer SVG-Attributen.
3. Keine neuen Abhängigkeiten, keine Craft-Assets, keine externen Fonts/URLs.
4. Alle Mängel aus [13](13-current-state.md) behoben oder begründet offen.
5. Kontrastpaare aus [03](03-colors.md) im Token-Test.
6. `ATTRIBUTION.md` nennt jeden übernommenen Launcher-Wert ([12](12-launcher-reference.md)).
7. Abweichungen von dieser Doku im Bericht begründet.

Danach prüft der Koordinator: `cargo xtask ci`, echter Browser ohne CSP-Fehler, axe mit Kontrast, Screenshots jeder Ansicht und jedes Dialogs (Upload in allen Phasen) hell/dunkel × EN/DE × 1280/360 px.

## Ergebnisbericht

Kurz: geänderte Dateien, ausgeführte Befehle mit Exitcode, offene Punkte, Abweichungen.

## Bewusst nicht Teil dieses Durchgangs

Zeilenmenüs (⋮), Sortierung, Suche, relative Zeitangaben, neue Ansichten (Organisation/Workspace anlegen, Quoten), Webfonts, Illustrationen. Das sind Funktionsänderungen mit eigenen Tickets.
