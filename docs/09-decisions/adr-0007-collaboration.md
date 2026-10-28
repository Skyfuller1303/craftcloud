# ADR-0007: FigCraft Collaboration nicht neu schreiben

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

`figcraft-server` besitzt persistente Live Sessions, WebSocket, Tokens, Team Libraries, aber keine Accounts. Eine neue Live Engine wäre Scopeverdopplung.

## Entscheidungsvorschlag

In MVP nur File/version storage. In P5 gewarteter Adapter mit per-person AuthZ capability und kompatiblem Protocol-Handshake.

## Konsequenzen und Risiken

Abhängigkeit von FigCraft-Review und Protokollevolution; deutlich weniger Duplikation und Datenrisiko.

## Betrachtete Alternativen

Share-Link Copy als Übergang; eigenes CRDT-Backend würde hohen Zeitbedarf und Divergenz verursachen.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
