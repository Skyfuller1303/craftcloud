# 06 — Elemente

Gemeinsam für alle Elemente:
- Radius 0.
- Fokus: `:focus-visible { outline: var(--focus-ring) solid var(--accent-ink); outline-offset: 2px; }`. In Navigation, Tabellenzellen und Listen `outline-offset: -2px`.
- Übergänge nur für `background-color`, `color`, `border-color`, Dauer `--motion` (120 ms, `ease-out`), bei reduzierter Bewegung 0.
- Deaktiviert: `opacity: .5; cursor: not-allowed`; keine Hover-Änderung.
- Klassen nach BEM: `block`, `block__element`, `block--modifier`.

---

## Buttons

Basis `.button`: `display: inline-flex; align-items: center; justify-content: center; gap: 8px; height: var(--control-height); padding: 0 16px; min-width: 80px;` Button-Schrift (mono 11 px 700, GROSS, 0.12 em), `white-space: nowrap`, Rahmen 1 px.

| Variante | Klasse | Ruhe | Hover | Gedrückt (`[data-pressed]`, `:active`) |
|---|---|---|---|---|
| Sekundär (Standard) | `.button` | Fläche transparent, Rahmen `--control-border`, Text `--ink` | Fläche `--invert-bg`, Rahmen `--invert-bg`, Text `--invert-fg` | Fläche `color-mix(in srgb, var(--invert-bg) 85%, var(--bg))` |
| Primär | `.button--primary` | Fläche `--invert-bg`, Rahmen `--invert-bg`, Text `--invert-fg` | Fläche 80 % (`color-mix(in srgb, var(--invert-bg) 80%, var(--bg))`) — Launcher `gamma_multiply(0.8)` | 70 % |
| Gefahr | `.button--danger` | Fläche `--bad`, Rahmen `--bad`, Text `--danger-fg` | Fläche 85 % | 75 % |
| Gefahr leise | `.button--danger-quiet` | transparent, Rahmen `--bad`, Text `--bad` | Fläche `--bad`, Text `--danger-fg` | Fläche 85 % |
| Leise | `.button--quiet` | transparent, Rahmen transparent, Text `--muted` | Fläche `--sunken`, Text `--ink-strong` | Fläche `--sunken` |
| Link | `.button--link` | kein Rahmen, keine Höhe/Breite-Vorgabe, Fließtext-Stil (body 14 px, normal geschrieben), `--accent-ink`, `text-decoration: underline; text-underline-offset: 2px` | Text `--ink-strong` | – |

Größen: `.button--sm` Höhe 28 px, Innenabstand 0 10 px, Mindestbreite 0. `.button--block` volle Breite.

Regeln:
- Eine Primäraktion je Seite/Dialog/Formular.
- Dialog-Buttons Mindestbreite 90 px (Launcher OK/Close/Cancel 90).
- Gefährliche Auslöser in Tabellen und Listen (In den Papierkorb, Entfernen, Verlassen, Widerrufen, Einladung verwerfen): `.button--danger-quiet .button--sm`. Bestätigung im Dialog: `.button--danger`.
- Ladezustand: vorhandener Text („Wird gespeichert…“), Breite darf nicht springen → ausreichende `min-width` je Kontext (z. B. Login-Button volle Breite).
- `<a class="button">` (Download, Export) bekommt dieselben Klassen; `text-decoration: none`.
- Externe Links mit nachgestelltem `↗` (Launcher `hud_link`).
- RAC `Button` hat `data-hovered`, `data-pressed`, `data-focus-visible`, `data-disabled`: Stile auf diese Attribute **und** native Pseudoklassen schreiben (native `<button>`/`<a>` kommen auch vor).

## Icon-Buttons

`.icon-button`: 34 × 34 px (`--sm`: 28 × 28), Rahmen 1 px `--line`, Fläche transparent, Glyphe 16 px `--muted`. Hover: Fläche `--invert-bg`, Rahmen `--invert-bg`, Glyphe `--invert-fg` (Launcher `glyph_button`, `widgets.rs:447-473`). Immer `aria-label`; sichtbarer Tooltip über RAC `TooltipTrigger`/`Tooltip` (Fläche `--invert-bg`, Text `--invert-fg`, 12.5 px, Innenabstand 4 px 8 px, Verzögerung 500 ms). Einsatz: Dialog schließen, Banner schließen, Link kopieren (zusätzlich zum Textbutton nur, wenn Platz knapp ist).

## Textfelder

Struktur (RAC `TextField`): `.field` > `.field__label` + `.field__input` + (`.field__hint` | `.field__error`).

- `.field`: `display: grid; gap: 4px;`
- `.field__label`: HUD-Stil, `--muted`.
- `.field__input`: Höhe 34 px, Innenabstand 0 12 px, Fläche `--sunken`, Rahmen 1 px `--control-border`, Text body 14 px `--ink`, Platzhalter `--muted`, `width: 100%`.
  - Hover: Rahmen `--ink`.
  - Fokus: Rahmen `--accent-ink` + Fokusring.
  - Ungültig (`[aria-invalid="true"]`, RAC `[data-invalid]`): Rahmen `--bad`.
  - Schreibgeschützt: Rahmen `--line`.
- `.field__hint`: `--text-small` `--muted`.
- `.field__error`: `--text-small` `--bad`, mit Statusquadrat 6 × 6 px `--bad` davor (Pseudo-Element, `aria-hidden` ist implizit).
- Mono-Variante `.field__input--mono`: `--font-mono` 13 px.
- Datum (`type="date"`): gleicher Stil, `color-scheme: light` bzw. `dark` passend zum Thema.
- Datei (`type="file"`): `::file-selector-button` im Sekundärbutton-Stil (Höhe 34, mono GROSS), Dateiname daneben `--muted`.

## Select

Zwei Implementierungen müssen identisch aussehen: RAC `ContextSelect` und native `NativeSelect`.

- Auslöser `.select__trigger` (RAC `Button`) bzw. `.select__native` (`<select>`): Höhe 34 px, Innenabstand 0 36 px 0 12 px, Fläche `--sunken`, Rahmen 1 px `--control-border`, Text body 14 px. Chevron (`Icon name="chevron-down"`) rechts 12 px, `--muted`, `pointer-events: none`.
  - Nativ: `appearance: none`; Chevron als echtes SVG-Element in einem Wrapper `.select__control { position: relative }` (keine `background-image` mit Data-URL — CSP verbietet `data:` nicht für Bilder, aber das Icon soll aus derselben Komponente kommen).
- Label: HUD-Stil über dem Auslöser (wie Feld).
- Popover (RAC `Popover`): Fläche `--bg`, Rahmen 1 px `--line-strong`, Schatten `--shadow`, Innenabstand 4 px, `min-width: var(--trigger-width)`, `max-height: 18rem; overflow: auto`.
- Eintrag (`ListBoxItem`): Höhe 32 px, Innenabstand 0 12 px 0 32 px, Text body 14 px. Hover/Fokus (`[data-focused]`, `[data-hovered]`): Fläche `--sunken`, Text `--ink-strong`. Ausgewählt (`[data-selected]`): `check`-Icon in der linken 32-px-Spalte, Gewicht 600.
- **Header-Variante** `.select--header`: kein Label sichtbar (visuell versteckt), Auslöser Höhe 48 px, Fläche transparent, kein Rahmen, Innenabstand 0 36 px 0 16 px, Wert im HUD-Stil `--ink`, Hover `--sunken`, Mindestbreite 160 px, maximale Breite 240 px mit Auslassung.

## Checkbox

RAC `Checkbox`: `.checkbox` (`display: inline-flex; align-items: center; gap: 10px; min-height: 34px;`) > `.checkbox__box` (16 × 16 px, Rahmen 1 px `--control-border`, Fläche `--sunken`) + Labeltext body 14 px `--ink`. Ausgewählt (`[data-selected]`): Fläche `--invert-bg`, Rahmen `--invert-bg`, `check`-Icon 12 px in `--invert-fg`. Fokus: Ring um die Box. Launcher `icon_spacing` 10.

## Segmente

Launcher `cells` (`widgets.rs:341-395`). `.segments` (`display: inline-flex; border: 1px solid var(--line)`) > `.segments__item` (Höhe 34 px, Innenabstand 0 12 px, HUD-Stil Gewicht 500, `--muted`, Trennlinie 1 px `--line` links ab dem zweiten Element). Hover: `--sunken`, `--ink-strong`. Ausgewählt (`[aria-current]`, `[aria-pressed="true"]`, `[data-selected]`): `--invert-bg`/`--invert-fg`. Optional Zähler mit 8 px Abstand in 60 % der Textfarbe.

Einsatz: mobile Navigation. Optional Theme-Wahl (System/Hell/Dunkel), **nur** wenn die zugängliche Rolle erhalten bleibt (heute nativer Select „Thema“; Tests prüfen ihn ggf. per Rolle `combobox` → dann Select belassen).

## Tabellen

Launcher-Tabellenansicht (`views.rs:477-632`) als Vorbild.

- `.table-wrap`: Rahmen 1 px `--line`, Fläche `--bg`, `overflow-x: auto` (nur als Fallback; schmal werden Karten).
- `.table`: `width: 100%; border-collapse: collapse;` body 14 px.
- Kopf `thead th`: Höhe 40 px, Innenabstand 0 16 px, HUD-Stil `--muted`, `text-align: left`, `white-space: nowrap`, untere Haarlinie.
- Zellen `td`, `tbody th`: **`height: 48px`** (nicht `min-height` auf `tr` — wirkt nicht), Innenabstand 8 px 16 px, `vertical-align: middle`, untere Haarlinie `--line`; letzte Zeile ohne Linie.
- Zeilen-Hover: Fläche `--sunken` (`tbody tr:hover`).
- Zeilenkopf `tbody th[scope="row"]`: 14 px, 600, `--ink-strong`; optionale zweite Zeile `.cell__sub` `--text-small` `--muted`.
- `.numeric`: rechtsbündig, `tabular-nums`; Kopf der Spalte ebenfalls rechtsbündig.
- `.code`: `--font-mono` 13 px. Gekürzte Hashes mit vollem Wert in `title` und im zugänglichen Text (z. B. visuell verstecktes Suffix).
- Leere Werte: „—“ `--muted`.
- Status-Spalten: Badge.
- Aktionsspalte `.table__actions`: rechtsbündig, `display: flex; gap: 8px; justify-content: flex-end; flex-wrap: nowrap`, Buttons `--sm`. Reihenfolge: harmlose Aktionen, dann gefährliche.
- `col--optional`: unter 1100 px `display: none` (Kopf und Zellen).
- **Karten unter 640 px:** `thead` visuell versteckt (nicht `display: none`), `tr` als Block mit Rahmen 1 px `--line`, 12 px 16 px Innenabstand, 8 px Abstand zwischen Karten; `td` als Raster `grid-template-columns: 40% 1fr` mit `td::before { content: attr(data-label); }` im HUD-Stil; Zeilenkopf oben über volle Breite; Aktionen unten, umbrechend, volle Breite pro Button bei Bedarf. Dafür bekommt jedes `td` im Markup `data-label={t("Spaltenname")}` (vorhandene Kopf-Texte wiederverwenden).

## Badges

Launcher `badge` (`widgets.rs:282-290`). `.badge`: `display: inline-flex; align-items: center; gap: 6px; height: 20px; padding: 0 6px;` Fläche `--sunken`, Rahmen 1 px `--line`, Badge-Schrift (mono 10 px 500, GROSS, 0.1 em), `white-space: nowrap`. Optional `.badge__dot` 6 × 6 px in Tonfarbe.

| Ton-Klasse | Textfarbe | Zuordnung |
|---|---|---|
| `.badge--good` | `--good` | angenommen, `success`, OK, aktuell |
| `.badge--warn` | `--warn` | eingeschränkt (`degraded`), läuft bald ab |
| `.badge--bad` | `--bad` | `denied`, `failed`, ausgefallen (`down`), widerrufen |
| `.badge--info` | `--accent-ink` | offen, wiederhergestellt |
| `.badge--neutral` | `--muted` | abgelaufen, nicht mehr gültig, nicht eingerichtet, Rollen, „Noch kein Inhalt“ |

Komponente `Badge({ tone, children })` in `src/components/Badge.tsx`. Ersetzt `.status--*`.

## Tags

Launcher `tag` (`widgets.rs:74-81`). `.tag`: Innenabstand 4 px 8 px, Tag-Schrift (mono 11 px 700, GROSS, 0.15 em), Fläche voll, kein Rahmen. `.tag--invert` (`--invert-bg`/`--invert-fg`) nur für die Wortmarke. Keine weiteren Einsätze in diesem Durchgang.

## Faktenlisten

`<dl class="facts">`: `display: grid; grid-template-columns: minmax(8rem, 12rem) 1fr; max-width: var(--form-max);` Jede Zeile (`dt` + `dd`) Höhe mindestens 40 px, `border-bottom: 1px solid var(--line)`, `align-items: center`. `dt` HUD-Stil `--muted`, `dd` 14 px `--ink`, `overflow-wrap: anywhere`, Badges inline. Unter 640 px einspaltig (`dt` über `dd`, 4 px Abstand).

## Dialoge

RAC `ModalOverlay` > `Modal` > `Dialog`.

- `.overlay`: `position: fixed; inset: 0; display: grid; place-items: center; padding: 16px; background: var(--scrim);`
- `.modal`: Fläche `--bg`, Rahmen 1 px `--line-strong`, Schatten `--shadow`, `max-height: calc(100vh - 32px); overflow: auto`.
- Breiten: `.modal--sm` 440 px (Bestätigung, Rolle), `.modal--md` 560 px (Formulare, Upload, Person hinzufügen), `.modal--lg` 760 px (Versionen). Jeweils `width: min(<Breite>, 100%)`. Launcher: 420–560 px.
- `.modal__dialog`: Innenabstand 24 px (< 640 px: 20 px), `display: grid; gap: 16px;`
- Kopf: `.modal__title` (`--text-dialog`), optional `.modal__subtitle` (HUD `--muted`), optional Icon-Button `close` rechts oben (klein), wenn der Dialog schließbar ist.
- Inhalt: Erklärtext `--muted`, Gegenstand (z. B. Dateiname) `--ink-strong`.
- Fuß `.modal__actions`: `display: flex; justify-content: flex-end; gap: 8px; margin-top: 8px;` **Abbrechen links, bestätigende Aktion rechts** (Launcher `dialogs.rs:168-177`). DOM-Reihenfolge entspricht der visuellen Reihenfolge (Tab-Reihenfolge!). Tests, die per Name suchen, bleiben gültig; Tests, die per Index suchen, auf Namen umstellen.
- Unter 640 px: Bottom-Sheet: `.overlay { place-items: end stretch; padding: 0 }`, `.modal { width: 100%; max-height: 90vh; border-width: 1px 0 0 }`, Buttons untereinander in voller Breite, bestätigende Aktion **oben** (`flex-direction: column-reverse`).

## Banner

Ersetzt `.alert`. Komponente `Banner({ tone, title?, children, actions?, onDismiss? })` mit kompatiblem Ersatz für `Alert` (gleiche Props `tone` = `error|info|success` werden auf `bad|info|good` abgebildet; Rollen bleiben: `role="alert"` bei Fehler, sonst `role="status"`).

`.banner`: `display: flex; gap: 12px; align-items: flex-start; padding: 10px 16px;` Fläche Tint der Tonfarbe ([03](03-colors.md#abgeleitete-tints)), Rahmen 1 px Tonfarbe 60 %, Text `--ink` 14 px. Links Statusquadrat 6 × 6 px in Tonfarbe (vertikal mittig zur ersten Zeile). Optionaler Titel 600 `--ink-strong`. Aktionen rechts (`margin-left: auto`, Buttons `--sm`), Schließen als Icon-Button `--sm`. Volle Breite des Inhalts, maximal `--content-max`; in Auth-Karten volle Kartenbreite.

## Kopierfeld (Einmal-Link)

`.copy-box`: Rahmen 1 px `--line-strong`, Fläche `--sunken`, Innenabstand 16 px, `display: grid; gap: 8px;` HUD-Label oben, Hinweis `--text-small` `--muted`, dann `.copy-box__row` (`display: flex`): Mono-Feld (schreibgeschützt, `flex: 1`, Rahmen `--line`) direkt angrenzend an Primärbutton „Link kopieren“ (gemeinsame Kante: `margin-left: -1px`), dahinter mit 8 px Abstand Leise-Button „Link ausblenden“. Nach Kopieren: vorhandene Bestätigung + Statusquadrat `--good`. < 640 px: Feld volle Breite, Buttons darunter.

## Fortschritt

Launcher `progress` (`widgets.rs:398-425`). `.progress` = Wrapper: oben Zeile mit Beschriftung `--text-small` `--muted` links und Zählwert `tabular-nums` rechts, 6 px Abstand, darunter natives `<progress>` mit `appearance: none; width: 100%; height: 4px; border: 0; background: var(--line);` Füllung `--accent` über `::-webkit-progress-bar`, `::-webkit-progress-value`, `::-moz-progress-bar`. Unbestimmt (`<progress>` ohne `value`): Segment 30 % Breite, Animation `translateX` 0,8 Durchläufe/s (siehe [09](09-motion-and-feedback.md)).

Upload-Schrittliste `.steps`: Liste ohne Aufzählungszeichen, je Schritt 24 px hoch, Statusquadrat 6 px + Text 14 px. Erledigt: Quadrat `--good`, Text `--muted`. Aktuell: Quadrat `--accent`, Text `--ink-strong`. Offen: Quadrat `--line-strong`, Text `--muted`. Fehler: Quadrat `--bad`.

## Leerzustand

`EmptyState({ children, action? })`: `.empty`: `padding: 40px 16px; text-align: center;` Text HUD-Stil `--muted` (Launcher „No apps match“, `app.rs:616-619`), optional Aktion darunter mit 12 px Abstand (Sekundärbutton). Innerhalb einer Tabellen-Fläche ersetzt der Leerzustand die Tabelle (gleicher Rahmen `.table-wrap`).

## Toasts

`Toaster` (rein visuell, [09](09-motion-and-feedback.md#toasts)): Container `.toasts` `position: fixed; right: 20px; bottom: 20px; display: grid; gap: 6px; width: min(380px, calc(100vw - 32px)); z-index: 30;` Toast `.toast`: Fläche `--bg`, Rahmen 1 px `--line-strong`, Schatten, Innenabstand 10 px 14 px, `display: flex; gap: 10px;` Statusquadrat 6 px (`--good`/`--bad`/`--accent`), Text 14 px `--ink`. Klick schließt.

## Links im Text

`a` im Fließtext: `--accent-ink`, Unterstreichung 1 px, Abstand 2 px; Hover `--ink-strong`. Besuchte Links gleiche Farbe.

## Fokus-Management (bestehend, beibehalten)

`ScreenHeading` fokussiert die h1 beim Wechsel; Dialoge geben den Fokus an den Auslöser bzw. `#main` zurück. Die h1 bekommt bei Fokus **keinen** Ring (`[tabindex="-1"]:focus { outline: none }`), wie bisher.
