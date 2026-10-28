# ADR-0002: Modularer Monolith statt Microservices

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

MVP zielt auf kleine Studios und Self-Hosting. Mehrere unabhängige Dienste erhöhen Proxy, Queue, Deployment und Fehlersuchkosten.

## Entscheidungsvorschlag

Ein Rust-Service mit modularen Crates und getrennten Core/Auth/Storage/API-Schichten, PostgreSQL und persistenten Blobs.

## Konsequenzen und Risiken

Eine Instanz bleibt zunächst Single Point of Failure; später Skalierung durch klare Domain-Boundaries möglich.

## Betrachtete Alternativen

Microservices ab Tag 1; verworfen wegen ops Aufwand und fehlender Lastbelege.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
