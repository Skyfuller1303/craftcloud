# 09 — Bewegung und Rückmeldung

## Bewegung

- Token `--motion: 120ms`, Kurve `ease-out`.
- Animiert werden nur: Farben (`background-color`, `color`, `border-color`), Toast-Ein-/Ausblenden (`opacity`), unbestimmter Fortschritt (`transform`).
- Nicht animiert: Layout, Dialog-Erscheinen (kein Einfliegen/Zoomen), Navigation, Tabellen.
- `@media (prefers-reduced-motion: reduce)`: `--motion: 0ms`, alle `animation` aus, unbestimmter Fortschritt zeigt ein stehendes Segment (der Launcher lässt Fortschritt weiterlaufen, `motion.rs:4`; im Web halten wir ihn an, WCAG 2.3.3).
- Launcher-Bezug: Hover-Effekte dort ca. 83 ms (egui Standard), Karten-Hover 300 ms; wir nutzen einheitlich 120 ms.

## Unbestimmter Fortschritt

Launcher `progress` (`widgets.rs:398-425`): Segment 30 % der Spurbreite, 0,8 Durchläufe pro Sekunde, nahtlos.

```scss
.progress--indeterminate::after {
  content: ""; position: absolute; inset: 0 auto 0 0; width: 30%;
  background: var(--accent); animation: progress-slide 1.25s linear infinite;
}
@keyframes progress-slide { from { transform: translateX(-100%); } to { transform: translateX(333%); } }
```

## Laden

- **Erstes Laden einer Liste:** 2-px-Leiste (unbestimmt) oben im Tabellenrahmen bzw. unter dem Seitenkopf, darunter der vorhandene Ladetext `--muted` (bleibt Live-Region `role="status"`). Keine Skelett-Platzhalter.
- **Nachladen („Mehr laden“):** Button zentriert **unter** der Tabelle mit 16 px Abstand, während des Ladens deaktiviert mit vorhandenem Text „Wird geladen…“. Heute steht er in `ListStatus` teils über der Tabelle — verschieben (Markup), Logik bleibt.
- **Aktionen in Buttons:** vorhandener Ladetext im Button, Button deaktiviert.

## Leer

`EmptyState` ([06](06-components.md#leerzustand)): HUD-Text `--muted`, 40 px oben, zentriert, optional eine Sekundäraktion. Ersetzt die leere Tabelle vollständig.

## Fehler

- Listen und Ansichten: Banner `bad` mit vorhandenem Fehlertext und den vorhandenen Aktionen „Neu laden“ / „Erneut versuchen“ (sekundär `--sm`) rechts im Banner. 429-Countdown bleibt im Buttontext.
- Felder: Feldfehler unter dem Feld ([06](06-components.md#textfelder)); `focusFirstInvalid` bleibt.
- Dialoge: Banner `bad` oben im Dialoginhalt.

## Erfolg

- Banner `good` in der Shell wie bisher (z. B. nach Annahme einer Einladung).
- Zusätzlich Toast (siehe unten) für kurze Bestätigungen, die heute nur unsichtbar angesagt werden.

## Toasts

Ziel: sichtbare, kurze Bestätigung für Aktionen, die heute nur über die unsichtbare Live-Region (`Announcer`) gemeldet werden (z. B. „Rolle geändert“, „In den Papierkorb verschoben“).

- Quelle: derselbe Aufruf, der heute `announce(...)` auslöst. Der `Announcer`-Kontext bekommt zusätzlich eine Liste sichtbarer Toasts; **keine neuen Texte**.
- Doppelte Ansage vermeiden: Toast-Container `aria-hidden="true"`; die Live-Region bleibt die einzige Ansage.
- Position unten rechts, 20 px Abstand, maximal 4 sichtbar, älteste fallen weg (Launcher `app.rs:259`).
- Dauer 4,5 s, Fehler 8 s (Launcher `app.rs:258`); Hover pausiert den Timer; Klick schließt.
- Ein-/Ausblenden nur `opacity` 120 ms.
- Unter 640 px volle Breite unten mit 16 px Rand.
- Tests: bestehende Tests, die Ansagen prüfen, bleiben gültig, weil die Live-Region unverändert bleibt.
