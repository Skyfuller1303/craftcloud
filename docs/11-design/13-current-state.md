# 13 — Ausgangslage

Stand Branch `feat/admin-design` (Basis `c8dc29d`, 2026-10-29). Lies den Code selbst; diese Liste zeigt, wo die Arbeit liegt.

## Dateien

- `src/styles/_tokens.scss` (≈108 Zeilen), `src/styles/main.scss` (≈864 Zeilen, ein Block).
- Komponenten `src/components/`: `Announcer`, `ContextSelect`, `Dialog`, `ErrorNotice`, `ListStatus`, `Preferences`, `RoleDialog`, `forms`.
- Ansichten `src/screens/`: `Shell`, `LoginScreen`, `SetupScreen`, `InviteScreen`, `ResetPasswordScreen`, `ProjectsScreen`, `FilesScreen`, `UploadDialog`, `VersionsDialog`, `MembersScreen`, `InvitationsScreen`, `ActivityScreen`, `SystemScreen`.
- Tests neben dem Code (`*.test.tsx`), darunter `a11y.test.tsx` (axe EN/DE) und `styles/tokens.test.ts` (Kontraste).
- Keine `style=`-Props im Quelltext (gut, so lassen).

## Bekannte Mängel

1. `main.scss` ist ein Block; Partials nach [14](14-workflow-and-acceptance.md#dateistruktur).
2. Doppelte Selektor-Blöcke: `.table` (13×), `.button` (3×), `.button--danger` (3×), `.status` (3×), `.alert`, `.progress`, `.shell__nav`, `.button--danger-quiet` (je 2×). Zusammenführen.
3. Zwei Status-Systeme (`.status--*` und `.alert`); ersetzen durch Badge und Banner ([06](06-components.md)).
4. `.select__label` eigens definiert (`main.scss` ≈ Zeile 236) statt gemeinsamer Label-Stil mit Feldern.
5. Rohfarbe für den Dialog-Hintergrund (`rgb(0 0 0 / 55%)`, ≈ Zeile 653) statt Token `--scrim`.
6. Tönungen (`color-mix`) direkt in Selektoren statt über das Tönungsrezept aus [03](03-colors.md).
7. Dialog-Fuß: Reihenfolge Abbrechen/Bestätigen uneinheitlich.
8. Gefährliche Zeilenaktionen teils als volle `danger`-Buttons, teils leise.
9. „Mehr laden“ teils über der Tabelle (`ListStatus`).
10. Leerzustände sind nackter Text ohne einheitliche Form.
11. Tabellen auf 360 px scrollen horizontal statt Kartenansicht.
12. Kein Favicon, keine Wortmarke nach [02](02-brand-and-logos.md).
13. Passwort-setzen-Fehlerfall nutzt sekundären statt primären Button.
14. Erfolgsmeldungen nur unsichtbar (Live-Region), keine Toasts.
15. Kein `prefers-reduced-motion`-Zweig.

## Teiländerungen aus früheren Codex-Läufen (nicht committet)

Zwei abgebrochene Läufe haben im Worktree hinterlassen:

- `_tokens.scss`: `--danger-fg`, einige Layout-Tokens (+12 Zeilen).
- `main.scss`: +132 Zeilen, u. a. Button-/Tabellen-Feinschliff.
- `tokens.test.ts`: zusätzliche Kontrastpaare (+18).
- `Dialog.tsx`: Abbrechen vor Bestätigen im DOM.
- `ProjectsScreen.tsx`, `FilesScreen.tsx`: Papierkorb-Buttons als `button--danger-quiet`.

Diese Änderungen passen grundsätzlich zur Vorgabe. Weiterverwenden, an die Tokens in [03](03-colors.md)/[05](05-spacing-and-layout.md) angleichen, nicht verwerfen. Nicht ungeprüft übernehmen: jeder Wert muss zu dieser Doku passen.
