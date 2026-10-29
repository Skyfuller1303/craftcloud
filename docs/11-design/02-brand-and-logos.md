# 02 — Marke, Wortmarke, Logos

## Rechtlicher Rahmen

- CraftCloud ist ein unabhängiger privater Prototyp. Er besitzt **kein** Recht an Namen, Logos oder Erscheinungsbild von ArtCraft oder den Craft-Apps (AGENTS.md Regel 2 und 8, ADR-0015).
- **Verboten:** das ArtCraft-Logo, das „A“-Zeichen, alle App-Icons (PhotoCraft, FilmCraft, …), Social-Icons aus dem Launcher, die Schriften Inter, Archivo und Geist Mono, Bilder und Grafiken von getartcraft.com, Nachbildungen davon (auch „ähnlich gezeichnet“).
- **Erlaubt:** Zahlenwerte aus dem MIT/Apache-lizenzierten Launcher-Code (Farben, Maße, Abstände, Schriftgrößen) mit Eintrag in `ATTRIBUTION.md`; der Stil (eckig, Haarlinien, Monospace-Labels) als Prinzip.

## Name und Wortmarke

- Produktname im Text: **CraftCloud** (ein Wort, C und C groß). In der Oberfläche heißt der Bereich „CraftCloud admin“ / „CraftCloud-Verwaltung“ (vorhandene Übersetzung).
- **Wortmarke in der Kopfleiste** = zwei Teile, rein typografisch, kein Bild:
  1. **Tag** `CRAFTCLOUD`: Monospace 11 px, Gewicht 700, Großbuchstaben, Sperrung 0.15 em, Innenabstand 4 px × 8 px, Fläche `--invert-bg`, Text `--invert-fg`, Radius 0 (Launcher-Tag „LAUNCHER“, `widgets.rs:74-81`).
  2. Daneben mit 12 px Abstand HUD-Text `ADMIN` in `--muted` (Launcher: „CRAFT APP SUITE“).
- Der Tag-Text ist Darstellung des vorhandenen Markennamens; der zugängliche Name des Bereichs bleibt der übersetzte Text („CraftCloud admin“). Umsetzung z. B. sichtbarer Tag mit `aria-hidden="true"` plus visuell versteckter übersetzter Text, oder der übersetzte Text wird per CSS als Tag gestaltet — ohne neuen Übersetzungsschlüssel, wenn möglich.
- Keine Farbe in der Wortmarke. Kein „Cloud“-Symbol.

## Favicon und Bildmarke

Heute: `<link rel="icon" href="data:," />` (leer). Neu erlaubt: **ein eigenes, originäres Favicon** als `apps/craftcloud-admin/public/favicon.svg`, eingebunden mit `<link rel="icon" href="/admin/favicon.svg" type="image/svg+xml" />` (gleiche Origin, CSP `img-src 'self'` erlaubt das; Vite kopiert `public/` in `dist/`, der Server liefert es unter `/admin/` aus).

Vorgaben für die Bildmarke:
- 32 × 32 `viewBox`, quadratisch, Ecken 0, monochrom.
- Geometrisch und eigenständig, abgeleitet aus dem Tag: gefülltes Quadrat in `#101014` mit einer weißen, aus Pfaden gezeichneten Form (z. B. zwei ineinandergreifende eckige „C“-Formen oder ein Quadrat mit ausgesparter Ecke). **Kein Text-Element** im SVG (Schriftabhängigkeit), keine Ähnlichkeit zu Craft-App-Icons (die sind abgerundete Quadrate mit Farbverläufen) oder zum ArtCraft-„A“.
- Optional ein 4 × 4-px-Quadrat in `#2d81ff` als einziger Farbakzent.
- Lesbar bei 16 px.
- In `ATTRIBUTION.md` als „originär für CraftCloud“ vermerken.

## Attribution

`ATTRIBUTION.md` enthält bereits die Tabelle der aus `crates/ui-egui/src/theme.rs` übernommenen Werte (Commit `7839c99924e7fc2854c6ed805f66f84332ffb83f`, Lizenz MIT OR Apache-2.0). Jeder **zusätzlich** übernommene Wert (z. B. Badge-Höhe 20 px aus `widgets.rs:282-290`, Toast-Breite 380 px aus `app.rs:728-763`, Dialog-Innenabstand 24 px aus `dialogs.rs:22-23`) wird dort mit Datei und Zeile ergänzt. Eigene Werte (z. B. `--control-border`, `--danger-fg`, Tabellenzeile 48 px) als „original to CraftCloud“ kennzeichnen.
