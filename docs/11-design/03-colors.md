# 03 — Farben

Alle Farben als CSS Custom Properties in `src/styles/_tokens.scss`, definiert für `:root[data-theme="light"]`, `:root[data-theme="dark"]` und ohne Attribut über `@media (prefers-color-scheme: dark)`. Quelle der Launcher-Werte: `crates/ui-egui/src/theme.rs:80-117` (`Tokens::DARK`, `Tokens::LIGHT`). Alpha-Werte aus 0–255 umgerechnet (z. B. 153/255 = 60 %).

## Palette

| Token | Hell | Dunkel | Herkunft | Verwendung |
|---|---|---|---|---|
| `--bg` | `#f2f1ee` | `#121316` | Launcher `bg` | Seite, Karten, Dialoge, Popover, Toasts |
| `--sunken` | `#e9e8e4` | `#060607` | Launcher `sunken` | Eingabefelder, Badges, Zeilen-Hover, Navigation-Hover, Kopierfeld |
| `--ink` | `#101014` | `#f2f1ee` | Launcher `ink` | Fließtext, Werte |
| `--ink-strong` | `#000000` | `#ffffff` | Launcher `ink_strong` | Überschriften, Zeilenköpfe, Hover-Text |
| `--muted` | `rgb(16 16 20 / 62%)` | `rgb(242 241 238 / 60%)` | Launcher `muted` | Sekundärtext, HUD-Labels, Tabellenköpfe, Hinweise |
| `--faint` | `rgb(16 16 20 / 42%)` | `rgb(242 241 238 / 38%)` | Launcher `faint` | **nur** Dekoration (Trenner-Punkte, deaktivierte Glyphen) — nie lesbarer Text |
| `--line` | `rgb(16 16 20 / 16%)` | `rgb(242 241 238 / 15%)` | Launcher `line` | Haarlinien, Tabellenrahmen, Icon-Button-Rahmen |
| `--line-strong` | `rgb(16 16 20 / 40%)` | `rgb(242 241 238 / 40%)` | Launcher `line_strong` | Dialog-, Popover-, Toast-Rahmen |
| `--control-border` | `rgb(16 16 20 / 58%)` | `rgb(242 241 238 / 40%)` | original (WCAG 1.4.11) | Rahmen von Eingaben, Selects, sekundären Buttons, Checkboxen |
| `--accent` | `#2d81ff` | `#2d81ff` | Launcher `accent` | Fortschrittsfüllung, Textauswahl, Info-Tint |
| `--accent-ink` | `#1659c4` | `#74aaff` | Launcher `accent_ink` | Links, Fokusring, Info-Text, aktive Sortierung |
| `--invert-bg` | `#101014` | `#ffffff` | Launcher `invert_bg` | Primärbutton, aktive Navigation, Hover sekundärer Buttons, Marken-Tag, Segment ausgewählt |
| `--invert-fg` | `#f2f1ee` | `#0b0b0c` | Launcher `invert_fg` | Text auf `--invert-bg` |
| `--good` | `#0a7563` | `#3fd6b8` | Launcher `good` | Erfolg |
| `--warn` | `#8a5800` | `#ffc14d` | Launcher `warn` | Warnung |
| `--bad` | `#b3261e` | `#ff6259` | Launcher `bad` | Fehler, Gefahr |
| `--danger-fg` | `#ffffff` | `#0b0b0c` | original | Text auf gefülltem `--bad` (Weiß auf `#ff6259` hätte nur ca. 2,9:1) |
| `--selection` | `--accent` | `--accent` | Launcher `selection` | `::selection` Hintergrund, Text weiß |
| `--shadow` | `0 8px 24px rgb(0 0 0 / 14%)` | `0 8px 24px rgb(0 0 0 / 55%)` | Launcher `shadow()` (Alpha 36 / 140) | Dialog, Popover, Toast |
| `--scrim` | `rgb(16 16 20 / 40%)` | `rgb(0 0 0 / 60%)` | original | Fläche hinter Dialogen |

## Abgeleitete Tints

Launcher `tint()` (`theme.rs:134-137`) setzt nur Deckkraft. Für Banner (Update-Banner im Launcher: Fläche 10 % dunkel / 7 % hell, Rahmen 60 %, `views.rs:138-139`):

```scss
--tint-strength: 7%;                       // hell
:root[data-theme="dark"] { --tint-strength: 10%; }
.banner--info { background: color-mix(in srgb, var(--accent) var(--tint-strength), var(--bg));
                border-color: color-mix(in srgb, var(--accent) 60%, transparent); }
```

Dasselbe für `--good`, `--warn`, `--bad`. Text auf Tint-Flächen ist `--ink` (Kontrast prüfen, siehe unten).

## Farbregeln

1. **Blau** (`--accent`, `--accent-ink`) nur: Links, Fokusring, Fortschritt, Info-Badge/-Banner, aktive Sortierung, Textauswahl. **Nie** für Primärbuttons, Navigation, Überschriften, Icons in Ruhe.
2. **Statusfarben** nur für Status: Badge-Text, Banner-Tint und -Rahmen, Statusquadrat, Validierungsfehler. Nie zur Dekoration.
3. **Lesbarer Text** mindestens `--muted`. `--faint` nie für Text, auch nicht für Platzhalter (Platzhalter: `--muted`).
4. **Hover** verdunkelt nicht per Filter, sondern wechselt Token (z. B. sekundärer Button → invertiert, Zeile → `--sunken`).
5. **Deaktiviert** = 50 % Deckkraft des ganzen Elements (Launcher `gamma_multiply(0.5)`, `widgets.rs:323-325`), plus `cursor: not-allowed`. Deaktivierte Elemente sind von WCAG-Kontrast ausgenommen, müssen aber als deaktiviert erkennbar sein.

## Kontrastpaare (müssen im Test geprüft werden)

Der Test `src/styles/tokens.test.ts` (bzw. `contrast.test.ts`) berechnet WCAG-Kontraste aus den Token-Werten. Er muss mindestens diese Paare in **beiden** Themes prüfen:

| Vordergrund | Hintergrund | Mindestwert |
|---|---|---|
| `--ink`, `--ink-strong`, `--muted` | `--bg` | 4.5 |
| `--ink`, `--muted` | `--sunken` | 4.5 |
| `--invert-fg` | `--invert-bg` | 4.5 |
| `--danger-fg` | `--bad` | 4.5 |
| `--good`, `--warn`, `--bad`, `--accent-ink`, `--muted` (Badge-Töne) | `--sunken` | 4.5 |
| `--accent-ink` (Links) | `--bg` | 4.5 |
| `--ink` | jeder Banner-Tint (berechnet als Mischung) | 4.5 |
| `--control-border` | `--bg` und `--sunken` | 3.0 |
| `--accent-ink` (Fokusring) | `--bg`, `--sunken`, `--invert-bg` | 3.0 |
| `--accent` (Fortschritt) | `--line`-Spur über `--bg` | 3.0 |

Transparente Tokens werden vor der Berechnung mit dem jeweiligen Hintergrund gemischt.
