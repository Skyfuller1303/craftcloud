# Incident Response und Fehlersuche

## Severity

- **SEV1**: bestätigter Tenant-Datenabfluss, nicht autorisierte Adminrechte, dauerhafter Datenverlust. Schreibzugriff sperren, Keys rotieren, Incident Response starten, betroffene Stakeholder nach geltenden Vorgaben informieren.
- **SEV2**: längerfristiger Ausfall, Storage-Integrity-Fehler, verwaiste Daten/ACL-Fehler ohne belegten Abfluss. Datenzustand stabilisieren, Backup sichern, Ursachenanalyse.
- **SEV3**: einzelne Fehlfunktionen, reduzierte Performance, UI-Bug, wiederholbare Fehler ohne Datenrisiko.

## Incident-Playbook

1. Umfang/Zeitraum/aktive Versionsstände feststellen; keine Logs mit Secrets nachproduzieren.
2. Security containment: betroffene Sessions/Tokens widerrufen, öffentliche Links sperren, Uploads in read-only schalten sofern nötig.
3. Eindeutigen Case-ID, Audit-/Request IDs, anonymisierte Metriken und Infrastrukturstatus sichern.
4. Recovery anhand konsistentem Backup; Integrität und Berechtigungen bestätigen.
5. Incident kommunizieren, Kundenkontakt gemäß festgelegten Datenschutzpflichten.
6. Ursachenanalyse, Regressionstest, Postmortem und Security Advisory über festgelegten Kanal.

## Kontrollfragen

Kann der Betreiber unterscheiden zwischen lokalem App-Fehler und Cloud-Fehler? Ist das Backup aktuell und geprüft? Können unvollständige Uploads gefahrlos bereinigt werden? Ist Login deaktiviert oder nur SSO ausgefallen? Sind alle Workspaces betroffen oder nur eine Organization? Keine Reset/Repair-Aktion, die lokale Dateien löscht.
