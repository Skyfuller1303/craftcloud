# Abgleich mit tatsächlichen `storytold`-Repositories (2026-10-28)

> **Quellenstatus:** Öffentlich sichtbare GitHub-Inhalte wurden stichprobenartig auf Repo-Wurzel, Readmes, AGENTS, Collaboration-Dokumentation und Contribution-Regeln geprüft. Kein lokaler Komplett-Checkout möglich, keine lückenlose Code-Audit-Zusage. Beobachtung (B) ≠ Entscheidung (E) ≠ Empfehlung (V) ≠ unbestätigt (U).

| Repo / offizieller Beleg | Beobachtung (B) | CraftCloud-Handhabung (V) | Status |
|---|---|---|---|
| [`wordcraft`](https://github.com/storytold/wordcraft) | Repo zeigt `apps/`, `crates/`, `docs/`, `packaging/`, `xtask/`, `AGENTS.md`, `CLAUDE.md`, `ROADMAP.md`, `ATTRIBUTION.md`, MIT/Apache | Cloud-Client wird separat in App-PR integriert, keine Änderung an Dokumentengine/Kommandosystem ohne Owner-Review | B/V |
| [`gridcraft`](https://github.com/storytold/gridcraft) | Entsprechendes Rust-Workspace-Layout, MIT/Apache und Fortschritts-/Agentenregeln | Keine serverseitige Tabellenformat-Interpretation im MVP | B/V |
| [`filmcraft`](https://github.com/storytold/filmcraft) | `apps/`, `crates/`, `docs/`, `packaging/`, `xtask/`, `AGENTS.md`, `CONTRIBUTING.md`, `SECURITY.md` | Große Medienfiles benötigen streamende Transfers und Quota; keinerlei Pflicht einer Cloud | B/V |
| [`photocraft`](https://github.com/storytold/photocraft) | modularer Rust-Workspace, dokumentierte Assets/Attribution und eigenständige Policies | Dateiablage neutral halten; nicht Dateiformate in Core koppeln | B/V |
| [`vectorcraft`](https://github.com/storytold/vectorcraft) | modularer Rust-Workspace mit separater Assets-/Brand-Provenienz, MIT/Apache | Trennung von Markenassets und technischem Code achten | B/V |
| [`wordcraft/AGENTS.md`](https://github.com/storytold/wordcraft/blob/main/AGENTS.md) | `ROADMAP.md`, `docs/gaps.md`, `docs/roadmap.md`, `docs/architecture.md`, Never-Crash und lokale Plan-Dateien | dieselbe Doku-/Statusdisziplin übernehmen; Engine-Kommandos bleiben app-spezifisch | B/V |
| [`figcraft`](https://github.com/storytold/figcraft) | `crates/`, `apps/`, `docs/`, `xtask/`, `packaging/`, `Cargo.toml`, `AGENTS.md`, `CLAUDE.md`, `ROADMAP.md`; MIT/Apache | Gleicher modularer Workspace-Stil, keine FigCraft-Speziallogik ins Core ziehen | B/V |
| [`figcraft/AGENTS.md`](https://github.com/storytold/figcraft/blob/main/AGENTS.md) | Kein `unsafe`, produktive Panics vermeiden, Layers, Tests, Originalassets, Attribution, Fortschrittsdokumentation | Lint- und Test-Gates übernehmen, Asset-Provenienz dokumentieren; App-Kommandos sind keine globale Cloud-Norm | B/V |
| [`figcraft/docs/server.md`](https://github.com/storytold/figcraft/blob/main/docs/server.md) | Eigenständiger persistenter WebSocket-Server, Token/Share-Links, Team Library, keine Accounts | Adapter statt Rewrite, keine Token-Übersetzung ohne Sicherheitsdesign | B/V |
| [`figcraft/docs/roadmap.md`](https://github.com/storytold/figcraft/blob/main/docs/roadmap.md) | Accounts/Teams in M9 offen, Multiplayer-Basis vorhanden | FigCraft-Maintainer für Auth-/Workspace-Verknüpfung einbeziehen | B/V |
| [`craft-launcher`](https://github.com/storytold/craft-launcher) | Rust-only egui, Apps+Crates, xtask, Packaging, dual license | Launcher **nicht** mit React/TS ausstatten; optionale Serverkonfiguration als späterer Integrations-PR | B/V |
| [`craft-launcher/AGENTS.md`](https://github.com/storytold/craft-launcher/blob/main/AGENTS.md) | Produktname, sichere Downloads, SHA-256, niemals User-Dateien blind löschen, nicht paniken, Farb-Token | Nur passende Sicherheits- und UX-Grundsätze; Launcher-Releases bleiben eigenständig | B/V |
| [`craft-libs`](https://github.com/storytold/craft-libs) | Gemeinsame Rust-Crates für Craft-Apps | Gemeinsames Client-Protokoll dort prüfen, aber keinen neuen Contract ohne Governance einführen | B/V |
| [`craft-ai-integrations`](https://github.com/storytold/craft-ai-integrations) | CONTRIBUTING, SECURITY, CI-orientierter Rust-Code, PR statt direkt main, conventional prefixes | PR-Flow, Test-first, Secretschutz und kleine Crates als Standard | B/V |
| [`artcraft-services`](https://github.com/storytold/artcraft-services) | Großer Rust-/TS-Monorepo, Actix, MySQL/Redis/Search/Objektspeicher; KI- und Medienbackend | Konzepte lernen, Backend nicht als `CraftCloud` ausgeben und beschränkt lizenzierten Code nicht kopieren | B/V |
| [`artcraft-services/LICENSE.md`](https://github.com/storytold/artcraft-services/blob/main/LICENSE.md) | ArtCraft License (WIP), kommerzielle Nutzung/Competitor-Nutzung eingeschränkt | Clean-room-Neuentwicklung und klärende Rechtsprüfung vor gemeinsamer Nutzung | B/V |
| `storytold/craftrules` | In FigCraft/Launcher-Agentenregeln erwähnt, öffentlich via GitHub 404 am Prüftag | Zugang/Standards erfragen; **vollständige Konformität derzeit nicht nachweisbar** | B/U |

## Was verbindlich zu übernehmen ist (vorläufige interne Qualitätsziele)

- Ein klarer `AGENTS.md`-Einstieg und schlanker `CLAUDE.md`-Verweis, README, `docs/` lowercase, Roadmap und verifizierte Checklisten.
- Rust-Monorepo mit konzeptionell getrennten Crates; Kern-Typen bleiben ohne Netzzugriff und UI-Abhängigkeit. Eigene Services dürfen React verwenden; die Desktop-Apps selbst bleiben entsprechend ihren eigenen Regeln rein Rust.
- Kein Produktions-Panic oder `unsafe`, limitierte Größen, sichere URL-/Dateipfad-Behandlung, testbare Fehlerfälle.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, Tests, migrationsgebundene Integrationstests und Security-Tests als geplante CI-Gates.
- Original-Assets und dokumentierte Herkunft; keine fremden Produktlogos/-fonts in einer neuen Repo.
- PR-Dokumentation, fokussierte Commits, Versions- und Contract-Strategie vor dem ersten externen Client.

## Was **nicht** automatisch übertragen werden darf

- Der GitHub-Orgname `storytold`, vorhandene Marken, Domains, Policies und Copyright-Inhaberschaft.
- FigCrafts lokale Command-Bus-Architektur als Vorschrift für einen REST-Backend-Service.
- `artcraft-services`-Datenbank-Schemata oder Security-Code unter einer uneingeschränkten Lizenz.
- Interne `craftrules`, die öffentlich nicht lesbar waren.
- Eine harte 1:1-Übernahme der GUI-Technologie: separates Admin-Web darf React/TS nutzen, ohne die Rust-only Craft-Apps zu verändern.

## Offene Konformitätsprüfung vor tatsächlichem Start

- [ ] Maintainer bestätigen gewünschte Repo-Struktur und Namensgebung.
- [ ] craftrules-Dokumente geprüft und Abweichungen dokumentiert.
- [ ] Lizenzentscheidung und Rechte an Name/Logo schriftlich geklärt.
- [ ] API-/SDK-Zuständigkeit mit `craft-libs`-/App-Maintainern abgestimmt.
- [ ] CI-/Branch-Protection-Vorgaben der Zielorganisation geklärt.
- [ ] Im jeweiligen App-Repo: eigene `AGENTS.md`- und Build-Gates vollständig beachten.
