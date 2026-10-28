# ADR-0003: PostgreSQL als primärer Store

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

Transaktionen für immutable revisions, CAS, members/RBAC, audit; Betreiber können PostgreSQL gut sichern.

## Entscheidungsvorschlag

PostgreSQL als offiziell getestete DB in MVP; SQLite optional nur nach eigenem Concurrency-/Backup-Design.

## Konsequenzen und Risiken

Zusätzlicher Container; weniger einfach als rein lokale SQLite, aber robustere Migrationen und Concurrent Writes.

## Betrachtete Alternativen

SQLite-first oder MySQL aus artcraft-services; beides möglich, aber nicht ohne eigene ADR und Tests.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
