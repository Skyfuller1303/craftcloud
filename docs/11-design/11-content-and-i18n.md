# 11 — Texte und Übersetzungen

## Grundregel

Die Gestaltung ändert **keinen** Text. Alle Texte kommen aus `src/locales/en.tsv` und `src/locales/de.tsv` (Format `context TAB source TAB translation`, englischer Quelltext als Schlüssel, ADR-0015). `npm run i18n:check` muss grün bleiben.

## Wenn ein neuer Text unvermeidbar ist

Beispiele: visuell versteckter Text für die Wortmarke, Tooltip eines neuen Icon-Buttons, Spaltenlabel für die Kartenansicht, das es noch nicht gibt.

1. Zuerst einen vorhandenen Text wiederverwenden (Spaltenköpfe existieren bereits und dienen als `data-label`).
2. Sonst: neuen Eintrag in **beiden** Dateien, gleicher Kontext, Platzhalter `{name}` identisch, Englisch als Quelltext.
3. Deutsch: Du-Form wie im Bestand („Diese Organisation hat keinen Workspace, den du sehen kannst.“), kurze Verben in Buttons.

## Schreibweisen in der Darstellung

| Regel | Umsetzung |
|---|---|
| Labels, Buttons, Badges, Navigation, Tags in Großbuchstaben | nur `text-transform: uppercase` |
| Fehlender Wert | „—“ (Gedankenstrich), `--muted` |
| Trenner zwischen Fakten | „ · “ |
| Aktionen mit Folgedialog | enden mit „…“, wenn der vorhandene Text das tut; nicht nachrüsten |
| Laufende Vorgänge | vorhandene Texte mit „…“ („Wird hochgeladen…“) |
| Externe Links | `↗` nach dem Text (Darstellung, kein neuer Übersetzungstext nötig: per CSS `::after` mit `content: " ↗"` und `aria-hidden`-Wirkung, da CSS-Inhalt von den meisten Screenreadern gelesen wird → stattdessen `<Icon name="external">` mit `aria-hidden`) |

## Rollen- und Statusnamen

Rollen und Status sind bereits übersetzt (Eigentümer, Administrator, Bearbeiter, Betrachter; Offen, Angenommen, Widerrufen, Abgelaufen, Nicht mehr gültig; OK, Eingeschränkt, Ausgefallen, Nicht eingerichtet). Badges zeigen genau diese Texte.

## Länge

Deutsche Texte sind bis zu 40 % länger. Buttons dürfen nicht abschneiden: `white-space: nowrap` plus ausreichend Platz; in engen Zeilen umbrechen die Buttons als Ganzes (`flex-wrap`), nicht ihr Text.
