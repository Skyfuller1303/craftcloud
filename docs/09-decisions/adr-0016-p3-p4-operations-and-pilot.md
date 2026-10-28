# ADR-0016: Betrieb in P3 und Pilot-App für P4

- **Status:** Accepted für den unabhängigen lokalen Prototyp (Projektverantwortlicher, 2026-10-28); bei Übergabe an Maintainer erneut zu prüfen
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps

## Kontext

P3 (CC-F11, CC-F14, Deployment) und P4 (CC-F13, D06) brauchen Festlegungen zu Reverse Proxy, Backup-Ziel, Audit-Aufbewahrung und der ersten integrierten Craft-App.

## Entscheidung

1. **Reverse Proxy:** Die Docker-Compose-Vorlage liefert **Caddy** mit (automatisches HTTPS, sichere Header). Eine nginx-Konfiguration wird nur als dokumentiertes Beispiel beschrieben.
2. **Backup:** `craftcloud-server admin backup` schreibt ein konsistentes Archiv (DB-Snapshot, Blobs, SHA-256-Manifest) in ein konfiguriertes lokales Verzeichnis, z. B. ein gemountetes NAS. Die Kopie nach außen (rclone, restic o. Ä.) verantwortet der Operator; das Runbook beschreibt es. Kein direkter S3-Upload in P3.
3. **Audit-Aufbewahrung:** Standard 1 Jahr, konfigurierbar. Personenbezogene Daten in Audit-Events werden minimiert, IP-Adressen standardmäßig nicht gespeichert.
4. **Pilot-App P4:** **FigCraft**. Integration nur als Dateiversionierung („Save a copy to CraftCloud“ / „Open from CraftCloud“) mit `.figcraft` als opakem Blob — ausdrücklich getrennt von der Live-Kollaboration über `figcraft-server` (CC-F16, ADR-0007). Änderungen nur auf einem lokalen Branch im FigCraft-Checkout bzw. dem Nutzer-Fork; kein PR an `storytold/figcraft` ohne ausdrückliche Freigabe. FigCrafts `AGENTS.md`, Build-Gates und lokale Workflows (`figcraft-dev`, `figcraft-ci`) gelten.

## Konsequenzen und Risiken

Caddy ist eine zusätzliche Komponente im Stack, spart aber Zertifikatsverwaltung. Backups ohne Off-site-Kopie schützen nicht vor Hostverlust; das Runbook muss das deutlich sagen. Bei FigCraft besteht Verwechslungsgefahr zwischen Cloud-Dateiversion und Live-Session; UI-Texte und Doku müssen beides klar trennen, und bestehende Share-Links dürfen nicht berührt werden.

## Betrachtete Alternativen

nginx oder kein Proxy in der Vorlage; direkter S3-Backup-Upload; 90 Tage oder unbegrenzte Audit-Aufbewahrung; PhotoCraft oder FilmCraft als Pilot.

## Review und Revision

Vor P4 den aktuellen FigCraft-Stand (Datei-Öffnen/Speichern, Places-Panel-Vorschläge wie `craft-libs#49`, CollabCraft-PRs) erneut prüfen, damit die Integration nicht mit laufender Upstream-Arbeit kollidiert.
