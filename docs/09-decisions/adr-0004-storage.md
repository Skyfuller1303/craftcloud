# ADR-0004: Opake Blobs und unveränderliche Revisionen

- **Status:** Proposed (bis Maintainer-Review nicht verbindlich)
- **Datum:** 2026-10-28
- **Owner:** noch zu benennen

## Kontext

Proprietäre/natives Craft-Formate dürfen nicht serverseitig interpretiert werden; parallele Editoren dürfen keine stillen Überschreibungen verursachen.

## Entscheidungsvorschlag

Upload-Session → checksum verified immutable blob → DB-CAS-Version-Commit; Local FS zuerst, S3 als Adapter.

## Konsequenzen und Risiken

Zusätzliche Storage-Manifest-/GC-Komplexität; dafür deterministische Konflikte/Backups.

## Betrachtete Alternativen

Einfacher PUT auf Dateiname/Pfad ohne Versionen; nicht geeignet wegen Datenverlustgefahr.

## Review und Revision

Im Phase-0-RFC gemeinsam mit Backend-, App- und Security-Maintainern prüfen. Nach Beschluss Status, Datum und Entscheidungsträger ergänzen; inkompatible Umsetzung erzeugt neue ADR statt stiller Umschreibung.
