# 01 — Grundsätze

## Charakter

Die Admin-Oberfläche ist ein **Werkzeug für Betreiber und Teams**, keine Marketingseite. Sie zeigt Listen, Formulare und Zustände. Das Vorbild, der Craft-Launcher, hat dafür eine klare Sprache, die wir übernehmen:

- **Eckig.** Radius 0 überall. Keine Pillen, keine runden Avatare, keine abgerundeten Karten.
- **Haarlinien statt Flächen.** Struktur entsteht durch 1-px-Linien (`--line`), nicht durch graue Kästen oder Schatten.
- **Monochrom.** Text in drei Abstufungen (`ink-strong`, `ink`, `muted`). Primäraktionen invertiert: schwarz auf hellem Grund, weiß auf dunklem.
- **Monospace-Großbuchstaben als Stimme.** Labels, Tabellenköpfe, Navigation, Buttons, Badges und Tags in Monospace, Großbuchstaben, gesperrt. Das ist der stärkste Wiedererkennungseffekt des Launchers.
- **Dicht, aber ruhig.** 34-px-Bedienelemente, 48-px-Tabellenzeilen, wenig Weißraum-Inseln, klare Ausrichtung an einer Kante.
- **Akzent sparsam.** Das Markenblau erscheint nur für Links, Fokus, Fortschritt und Info — nie für Buttons oder Navigation.

## Leitregeln

1. **Gleiche Rolle, gleiche Form.** Eine primäre Aktion sieht überall gleich aus; ein Status (gut, Warnung, Fehler, Info, neutral) wird überall als Badge derselben Art gezeigt.
2. **Eine Primäraktion pro Bereich.** Seite, Dialog oder Formular haben höchstens einen Primärbutton.
3. **Gefahr ist erkennbar, aber nicht laut.** Gefährliche Auslöser in Listen sind rot umrandet (`danger-quiet`), erst die Bestätigung im Dialog ist rot gefüllt.
4. **Information vor Dekoration.** Keine Illustrationen, Verläufe, Hintergrundbilder, Emoji, Glanzeffekte.
5. **Schatten nur für schwebende Ebenen:** Dialog, Popover, Toast.
6. **Alles über Tokens.** Komponenten verwenden nur CSS Custom Properties aus `_tokens.scss`; keine Rohfarben in Komponentenstilen.
7. **Hell und dunkel sind gleichwertig.** Jede Änderung wird in beiden Themes geprüft.
8. **Mobil ist Pflicht.** 360 px Breite ohne horizontales Scrollen der Seite.

## Do / Don't

| Do | Don't |
|---|---|
| Haarlinie unter Tabellenkopf und zwischen Zeilen | Zebra-Streifen, Kartenschatten |
| Status als Badge mit Text | Status nur als farbiger Text oder Punkt |
| Primärbutton invertiert (`--invert-bg`) | Primärbutton blau |
| Monospace-Großbuchstaben per CSS | Großbuchstaben im Übersetzungstext |
| „—“ für fehlende Werte | leere Zellen |
| Leerzustand mit HUD-Text | leere Tabelle mit nur Kopfzeile |
| eigene, einfache SVG-Icons | Icon-Fonts, Emoji, kopierte Icons |
| Systemschriften | Webfonts, besonders die Craft-Schriften |
