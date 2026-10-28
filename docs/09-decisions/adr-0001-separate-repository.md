# ADR-0001: Eigenständige Repo statt `artcraft-services` erweitern

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

`artcraft-services` kombiniert KI-Generierung, Credits, Billing, Media-Libraries und eine eigene beschränkte WIP-Lizenz. CraftCloud hat andere Nutzerreisen und einfache Self-hosting-Ziele.

## Entscheidungsvorschlag

Eigenständige neue Codebasis + dokumentierte API-Schnittstellen. Kein Quellcode oder DB-Schema aus `artcraft-services` kopieren; ggf. nach Lizenzfreigabe isolierte Interoperabilität.

## Konsequenzen und Risiken

Neuer Wartungsaufwand und Doppelungen in Auth-Konzepten, dafür minimale unabhängige Deployments und klarere Lizenz-/Produktgrenzen.

## Betrachtete Alternativen

Erweiterung des bestehenden Monorepos oder Einbau als `craft-libs`-Crate; beide würden Produkt- und Deploymentzuständigkeiten stärker koppeln.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
