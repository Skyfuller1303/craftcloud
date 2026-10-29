# 05 — Abstände, Maße und Layout

## Maß-Tokens

| Token | Wert | Verwendung | Herkunft |
|---|---|---|---|
| `--space-1` | 4 px | Label↔Feld, Popover-Innenabstand | Launcher `menu_margin` 4 |
| `--space-2` | 8 px | Button-Gruppen, Zeilenaktionen, Icon↔Text | Launcher `item_spacing.x` 8 |
| `--space-3` | 12 px | Toolbar-Elemente, Felder dicht, Tag↔HUD | Launcher Header 12 |
| `--space-4` | 16 px | Formularfelder, Zellen horizontal, Abschnittskopf↔Inhalt | Launcher Button-Padding 16 |
| `--space-5` | 20 px | Seitenrand horizontal | Launcher CentralPanel 20 |
| `--space-6` | 24 px | Dialog-Innenabstand, Abstand vor Dialog-Fuß | Launcher Modal 24 |
| `--space-7` | 32 px | Abstand zwischen Abschnitten | original |
| `--space-8` | 40 px | Leerzustand oben | Launcher Empty 40 |
| `--control-height` | 34 px | Buttons, Felder, Selects, Segmente, Navigationseinträge | Launcher `CONTROL_H` |
| `--control-height-sm` | 28 px | kleine Zeilenaktionen | original |
| `--header-height` | 48 px | Kopfleiste | Launcher `HEADER_H` |
| `--nav-width` | 208 px | Seitennavigation | original |
| `--row-height` | 48 px | Tabellenzeile | original (Launcher 62 mit Icons) |
| `--head-row-height` | 40 px | Tabellenkopf | Launcher 42 |
| `--hairline` | 1 px | alle Linien | Launcher `hairline()` |
| `--focus-ring` | 2 px | Fokusring | original |
| `--radius` | 0 | überall | Launcher `CornerRadius::ZERO` |
| `--content-max` | 1280 px | Hauptbereich maximal | original |
| `--form-max` | 560 px | Formulare, Faktenlisten | Launcher Dialog 560 |
| `--auth-card` | 400 px | Login-/Einrichtungs-Karte | original |

## Shell (angemeldet)

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│[CRAFTCLOUD] ADMIN │ ORGANISATION ▾ │ WORKSPACE ▾ │ ……………… │ DE ▾ │ SYSTEM ▾ │ Ada …  [LOG OUT]│ 48
├───────────────┬────────────────────────────────────────────────────────────────────────┤
│ ÜBERSICHT     │  Seitentitel                                         [PRIMÄRAKTION]      │
│▓PROJEKTE▓▓▓▓▓▓│  Beschreibung (muted, klein)                                             │
│ DATEIEN       │                                                                          │
│ MITGLIEDER    │  ┌ Toolbar ──────────────────────────────────────────────────────────┐  │
│ EINLADUNGEN   │  │ [Select ▾] [Feld        ]                 [□ Papierkorb anzeigen]   │  │
│ AKTIVITÄT     │  └───────────────────────────────────────────────────────────────────┘  │
│ ───────────── │  ┌ Tabelle ──────────────────────────────────────────────────────────┐  │
│ EINSTELLUNGEN │  │ NAME                  GEÄNDERT                           AKTIONEN  │  │
│ SYSTEM        │  │ Brand kit            12.10.2026          [DATEIEN] [IN PAPIERKORB] │  │
│               │  └───────────────────────────────────────────────────────────────────┘  │
│               │                           [MEHR LADEN]                                   │
└───────────────┴────────────────────────────────────────────────────────────────────────┘
```

### Kopfleiste

- Höhe 48 px, Fläche `--bg`, untere Haarlinie. Elemente stehen ohne Zwischenraum, Gruppen durch senkrechte 1-px-Linien (`--line`) in voller Höhe getrennt (Launcher `rule()`, `widgets.rs:84-88`).
- **Links:** Wortmarke ([02](02-brand-and-logos.md)) mit 20 px Innenabstand links und 16 px rechts, dann Linie.
- **Kontext:** Organisations- und Workspace-Select in Header-Variante ([06 §Select](06-components.md#select)): volle Höhe, keine Fläche, HUD-Text des Werts in `--ink`, Chevron, Hover `--sunken`. Zugängliche Labels bleiben (visuell versteckt).
- **Rechts** (rechtsbündig): Sprache und Thema (Header-Select-Variante, kompakt), Anzeigename (`--muted`, Auslassung), Button „Abmelden“ (sekundär, klein) mit 12 px Abstand zum Rand.
- **< 1100 px:** Sprache und Thema bleiben, Anzeigename entfällt (bleibt in Übersicht sichtbar).
- **< 640 px:** zwei Zeilen: (1) Wortmarke … Abmelden, (2) Organisation | Workspace je halbe Breite. Sprache/Thema wandern ans Ende der Navigation.

### Seitennavigation

- Breite 208 px, rechte Haarlinie, Innenabstand 12 px oben/unten, 0 seitlich, `position: sticky; top: 0` im Body-Bereich.
- Einträge: Höhe 34 px, Innenabstand 0 20 px, HUD-Stil (mono 11 px, GROSS, 0.15 em) `--muted`.
- Hover: Fläche `--sunken`, Text `--ink-strong`.
- Aktiv (`aria-current="page"`): Fläche `--invert-bg`, Text `--invert-fg` (Launcher `nav_link` aktiv). Kein Akzentbalken, kein Fettdruck nötig.
- Fokus: Fokusring nach innen (`outline-offset: -2px`), damit er nicht abgeschnitten wird.
- Gruppen: Inhalt (Übersicht, Projekte, Dateien, Mitglieder, Einladungen, Aktivität), dann 12 px Abstand, Haarlinie über volle Breite, 12 px Abstand, Verwaltung (Einstellungen, System). Heute steht „Einstellungen“ zwischen „Einladungen“ und „Aktivität“; die Einträge dürfen im Markup umsortiert und auf zwei `<ul>` in einem `<nav>` verteilt werden (Texte, Ziele und `aria-current` bleiben).
- **< 640 px:** Navigation wird eine horizontal scrollbare Segmentleiste direkt unter der Kopfleiste (Segment-Stil, [06](06-components.md#segmente)), aktiver Eintrag invertiert, `scroll-snap`, aktiver Eintrag wird beim Wechsel sichtbar gescrollt (nur CSS/`scrollIntoView` auf Fokus, keine Logikänderung).

### Hauptbereich

- Innenabstand: oben 18 px, seitlich 20 px, unten 40 px (Launcher 18/20).
- Inhalt linksbündig, maximale Breite `--content-max`.
- **Seitenkopf:** Zeile mit h1 links und Primäraktion rechts (`display: flex; justify-content: space-between; align-items: baseline; gap: 16px; flex-wrap: wrap`). Darunter optional Beschreibung `--text-small` `--muted`. 16 px bis zum Inhalt.
- **Hinweis-Banner** der Shell (z. B. Erfolgsmeldung) zwischen Seitenkopf und Inhalt, 16 px Abstand.
- **Abschnitte:** h2 (optional HUD-Eyebrow darüber, 4 px Abstand), sekundäre Abschnittsaktion rechts in derselben Zeile, 12 px bis Inhalt, 32 px zwischen Abschnitten; zwischen zwei Abschnitten eine Haarlinie mit 32 px Abstand oben (Abstand nach unten 24 px).
- **Formulare und Faktenlisten:** maximale Breite `--form-max`.
- **Toolbar:** Zeile mit 12 px Abstand, `flex-wrap: wrap`, `align-items: flex-end` (Felder mit Label oben bündig zu Buttons), Filter links, Umschalter/Sekundäraktionen rechts (`margin-left: auto`). Optional eingerahmt (1 px `--line`, 16 px Innenabstand) bei Filterformularen.

## Nicht angemeldete Seiten

- Kopfleiste 48 px: Wortmarke links, Sprache/Thema rechts.
- Zentrierte Karte: Breite `--auth-card` (Einrichtung 480 px), `max-width: calc(100% - 32px)`, Abstand oben `clamp(24px, 12vh, 120px)`, Fläche `--bg`, Rahmen 1 px `--line`, Innenabstand 32 px (< 640 px: 20 px).
- Inhalt: Titel (`--text-dialog`), Beschreibung `--muted`, 24 px, Banner falls vorhanden, Felder mit 16 px Abstand, 24 px, Primärbutton **volle Breite**, darunter sekundäre Aktionen als Link-Buttons zentriert, 12 px Abstand.

## Breakpoints

| Name | Bereich | Wichtigste Änderungen |
|---|---|---|
| wide | ≥ 1100 px | Standard |
| medium | 640–1099 px | Anzeigename weg, optionale Tabellenspalten (`col--optional`) ausgeblendet, Faktenraster einspaltig |
| narrow | < 640 px | Kopfleiste zweizeilig, Navigation als Segmentleiste, Tabellen als Karten, Dialoge als Bottom-Sheet in voller Breite, Toolbar-Elemente volle Breite |

Media Queries mit `rem` schreiben (`40rem` = 640 px, `68.75rem` = 1100 px), damit Zoom korrekt greift. 360 px und 200 % Zoom ohne horizontales Seiten-Scrollen.
