# ADR-0015: Admin-UI — Sprachen, Design-Tokens, Komponenten und Auslieferung

- **Status:** Accepted für den unabhängigen lokalen Prototyp (Projektverantwortlicher, 2026-10-28); bei Übergabe an Maintainer erneut zu prüfen
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps
- **Ergänzt:** ADR-0005 (React/TypeScript)

## Kontext

PR-012 baut die Admin-Oberfläche. Offen waren Sprachen, visueller Stil, Komponentenbasis und Auslieferung. Lokale Prüfung der Craft-Repositories (Stand 2026-10-28):

- Übersetzungen: `storytold/photocraft`, `storytold/filmcraft` und `storytold/craft-launcher` verwenden je Sprache eine TSV-Datei (`crates/ui-egui/src/i18n/<sprache>.tsv`) mit den Spalten `context <TAB> source <TAB> translation`; der englische Text im Code ist der Schlüssel, `@id` im Kontext überschreibt für eine Befehls-ID. `storytold/figcraft` nutzt dagegen TOML (`crates/ui-egui/locales/*.toml`), `effectcraft` Rust-Tabellen. Es gibt also keinen einheitlichen Standard, aber eine Mehrheit für TSV.
- Design: Es existiert kein gemeinsames Web-Design-System. Jede App hat ein eigenes egui-Theme. Der Launcher (`crates/ui-egui/src/theme.rs`) definiert benannte Farb-Tokens für Dunkel/Hell/System, eckige Ecken, Haarlinien, feste Abstände und Bewegung nach OS-Einstellung; seine `AGENTS.md` verlangt, Farben nie hart zu codieren.

## Entscheidung

1. **Sprachen:** Englisch (Standard und Fallback) und Deutsch, erweiterbar über Sprachdateien. Format wie in PhotoCraft/FilmCraft/Launcher: eine `<sprache>.tsv` je Sprache, Spalten `context <TAB> source <TAB> translation`, englischer Quelltext als Schlüssel, Kommentarzeilen mit `#`. Platzhalter `{name}` und Pluralformen werden in der Datei dokumentiert. Eine neue Sprache entsteht durch Hinzufügen einer Datei ohne Codeänderung; ein Check meldet fehlende oder überzählige Einträge. Sprache folgt dem Browser und ist manuell umschaltbar.
2. **Stil:** an den Craft-Apps orientiert, Hauptvorlage ist das Launcher-Theme. Übernommen werden die **Prinzipien und Token-Struktur** (benannte Tokens, Hell/Dunkel/System, eckige Ecken, Haarlinien, Abstände, reduzierte Bewegung). Keine Logos, App-Icons, Schriften oder Bilder der Craft-Apps; Systemschriften. Werden konkrete Zahlenwerte (Farben, Abstände) aus dem MIT/Apache-lizenzierten Launcher übernommen, wird jede Quelle in `ATTRIBUTION.md` mit Datei, Commit und Lizenz vermerkt.
3. **Komponenten:** Headless-Primitive für Barrierefreiheit (z. B. React Aria) plus eigenes **SCSS** mit Design-Tokens als CSS Custom Properties. Keine vollständige Komponentenbibliothek mit eigener Designsprache. Ziel WCAG 2.2 AA.
4. **Auslieferung:** `craftcloud-server` liefert den statischen Build unter `/admin` aus, gleiche Origin wie die API. Damit einfache Cookie-/CSRF-Regeln, strikte CSP, kein CORS.

## Konsequenzen und Risiken

Ein TSV-Katalog im Browser braucht einen kleinen eigenen Parser oder einen Build-Schritt zu JSON; beides wird getestet (Tabs, Escapes, fehlende Spalten). Der Craft-Bezug ist rein stilistisch; Markenrechte am Erscheinungsbild bleiben unberührt, solange keine Assets kopiert werden. Die Auslieferung durch den Server koppelt UI- und Server-Releases.

## Betrachtete Alternativen

JSON/ICU-Kataloge (Web-Standard, aber abweichend von den Craft-Apps); TOML wie FigCraft; neutrales eigenes Theme; vollständige Komponentenbibliothek (Mantine/MUI); Auslieferung separat über den Reverse Proxy.

## Review und Revision

Vor PR-012 die aktuelle Launcher-Version erneut prüfen (Theme kann sich geändert haben). Bei Übergabe an Maintainer klären, ob ein gemeinsames Web-Token-Set gewünscht ist.
