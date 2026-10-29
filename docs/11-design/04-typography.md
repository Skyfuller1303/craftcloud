# 04 — Typografie

## Schriftstapel

Keine Webfonts (Brand-Regel, siehe [02](02-brand-and-logos.md)). Zwei Systemstapel:

```scss
--font-body: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
--font-mono: ui-monospace, "Cascadia Mono", "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
```

Der Launcher nutzt Inter (Text), Archivo (Überschriften) und Geist Mono (Labels, Buttons, Badges). Wir ersetzen sie durch `--font-body` bzw. `--font-mono`. Die Überschriften bekommen statt Archivos Breite eine leicht negative Sperrung und Gewicht 650.

## Skala

| Rolle | Token | Größe / Zeilenhöhe | Stapel | Gewicht | Sperrung | Fall | Farbe | Launcher-Bezug |
|---|---|---|---|---|---|---|---|---|
| Seitentitel (h1) | `--text-title` | 24 px / 1.2 | body | 650 | −0.02 em | normal | `ink-strong` | Heading 24, Display |
| Dialogtitel | `--text-dialog` | 20 px / 1.25 | body | 650 | −0.02 em | normal | `ink-strong` | Dialogtitel 22 (`dialogs.rs:71-87`) |
| Abschnitt (h2) | `--text-section` | 16 px / 1.3 | body | 650 | −0.01 em | normal | `ink-strong` | Tabellentitel 16 |
| Fließtext | `--text-body` | 14 px / 1.5 | body | 400 | 0 | normal | `ink` | Body 14 |
| Betont | – | 14 px | body | 600 | 0 | normal | `ink-strong` | semibold |
| Klein / Hinweis | `--text-small` | 12.5 px / 1.45 | body | 400 | 0 | normal | `muted` | Small 12, Versionszeile 12.5 |
| HUD-Label | `--text-label` | 11 px / 1.3 | mono | 500 | 0.15 em | GROSS | `muted` | `.hud-label` 11 px, 0.15 em (`widgets.rs:29-52`) |
| Button | `--text-button` | 11 px / 1 | mono | 700 | 0.12 em | GROSS | je Variante | Button 11 px, 0.12 em (`widgets.rs:310-329`) |
| Badge | `--text-badge` | 10 px / 1 | mono | 500 | 0.1 em | GROSS | Tonfarbe | Badge 10 px, 1 px (`widgets.rs:282-290`) |
| Tag | `--text-tag` | 11 px / 1 | mono | 700 | 0.15 em | GROSS | je Variante | Tag (`widgets.rs:74-81`) |
| Code / IDs / Hash | `--text-code` | 13 px / 1.4 | mono | 400 | 0 | normal | `ink` | Monospace 13 |

## Wo welcher Stil

- **HUD-Label:** Feldlabels, Tabellenköpfe, Navigationseinträge, Abschnitts-Eyebrows, Faktenlisten-Begriffe (`dt`), Leerzustände, Untertitel in Dialogen.
- **Button-Stil:** alle Buttons und Button-Links, Segmentzellen (HUD-Stil, Gewicht 500 wie Launcher `cells`).
- **Code:** Aktionscodes im Audit, Request-IDs, Hashes, Datei-Formate (`app · format`), IDs, Einladungslink-Feld.
- **Fließtext:** alles andere.

## Großschreibung

Nur per `text-transform: uppercase`. Quelltexte und Übersetzungen bleiben normal geschrieben, damit Screenreader und Tests die Originaltexte sehen (Launcher macht es genauso, `widgets.rs:4,26`). Deutsche Großbuchstaben: `ß` wird in Großschreibung von Browsern korrekt zu „SS“ umgesetzt; `lang`-Attribut am `<html>` muss stimmen (wird bereits gesetzt).

## Zahlen und Datumsangaben

- Tabellenzahlen, Größen, Zeitangaben: `font-variant-numeric: tabular-nums`, rechtsbündig (Klasse `numeric`).
- Fehlender Wert: „—“ in `--muted`.
- Trenner zwischen Fakten: „ · “ (Launcher-Konvention). Nur in Darstellung, keine Textänderung.

## Lange Texte

- Namen in Tabellen: maximal zwei Zeilen, danach `text-overflow: ellipsis` mit vollem Text im zugänglichen Namen bzw. `title`.
- Anzeigename in der Kopfleiste: einzeilig mit Auslassung, maximale Breite 16 ch.
- `overflow-wrap: anywhere` für IDs und Hashes in Faktenlisten.
