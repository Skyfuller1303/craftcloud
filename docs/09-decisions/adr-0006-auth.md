# ADR-0006: Separate Cloud-Identität mit optionalem SSO

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

Cloud muss ohne zentrale ArtCraft-Plattform self-hostbar sein; Launcher und Apps bleiben ohne Login nutzbar.

## Entscheidungsvorschlag

Lokale Benutzer + sichere Web-Sessions; später OIDC; Desktop Device Flow als separate Security-Entscheidung; SSO nie Bedingung für lokale App.

## Konsequenzen und Risiken

Zunächst unabhängige Cloud-Accounts; spätere Federation benötigt Mapping und Account-Linking ohne E-Mail-Only.

## Betrachtete Alternativen

Pflichtmäßiger ArtCraft-Login oder direktes Übernehmen von `artcraft-services` Sessions; wegen lokaler Unabhängigkeit/Lizenz nicht empfohlen.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
