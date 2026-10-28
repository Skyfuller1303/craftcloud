# ADR-0010: axum als HTTP-Framework, MSRV 1.95, lokale CI

- **Status:** Accepted für den unabhängigen lokalen Prototyp (Projektverantwortlicher, 2026-10-28); bei Übergabe an Maintainer erneut zu prüfen
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps

## Kontext

ADR-0009 hielt das HTTP-Framework als offenen Punkt fest; `deployment.md` nannte zuvor unbegründet Actix. PR-001 (CC-P01-01) braucht ein Framework, eine deklarierte Mindest-Rust-Version und eine CI-Strategie. `quality-and-privacy-gates.md` verbietet eine erfundene Toolchain-Festlegung ohne Begründung; beobachtete Geschwister-Repos nutzen teils Rust 1.95.

## Entscheidung

1. `craftcloud-server` und `craftcloud-api` verwenden **axum** auf tokio, mit `tower`/`tower-http` für Request-ID, Body-Limits und Timeouts.
2. Der Workspace deklariert `rust-version = "1.95"`. Lokal gebaut wird mit der installierten stabilen Toolchain (derzeit 1.99.0). Eine MSRV-Prüfung mit 1.95 ist **nicht** Teil der lokalen Checks, solange diese Toolchain nicht installiert ist; das wird in jedem Testbericht offen ausgewiesen.
3. CI läuft vorerst **nur lokal** über `cargo xtask ci`; kein GitHub-Actions-Workflow im privaten Repository.

## Konsequenzen und Risiken

Abhängigkeiten müssen mit Rust 1.95 kompatibel bleiben, ohne dass das automatisch geprüft wird (Risiko: unbemerkte MSRV-Verletzung). Ohne Remote-CI hängt die Nachweisführung an lokal archivierten Logs.

## Betrachtete Alternativen

actix-web (ausgereift, eigenes Runtime-Modell, in `artcraft-services` verwendet — nur konzeptionell relevant); Entscheidung vertagen (PR-001 ohne Server). MSRV 1.99 (einfacher, weicht von Geschwister-Repos ab). GitHub Actions (verbraucht Actions-Minuten des Kontos).

## Review und Revision

Vor einer Übergabe an storytold-Maintainer erneut zur Diskussion stellen. MSRV-Prüfung nachrüsten, sobald eine 1.95-Toolchain verfügbar ist.
