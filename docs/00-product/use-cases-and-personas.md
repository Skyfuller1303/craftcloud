# Nutzerrollen und ausführbare End-to-End-Szenarien

## Persona A – Einzelanwender ohne Server

**Ausgangslage:** Offline, nur lokale Projekte. **Aktion:** App installieren, starten, lokale Datei öffnen, ändern und speichern. **Erwartung:** Kein Login, kein Server-Dialog, kein Tracking- oder Cloud-Verhalten ohne Opt-in. **Test:** Netzwerk sperren, frische Installation, Standard-Workflows ausführen. **Fehlerfall:** Eine zuvor verbundene CraftCloud ist offline; lokale Arbeitsdateien bleiben zugänglich.

## Persona B – Administrator einer Kreativagentur

**Ausgangslage:** Self-Hosted-Installation über Reverse Proxy mit persistentem Volumen. **Aktionen:** Setup Token einlösen, ersten Admin erstellen, Organisationsname und Workspace anlegen, Mitglied einladen, Storage-Quota setzen, Backup auslösen. **Erwartung:** Setup-Key ist einmalig, Einladungen sind zeitgebunden, sämtliche admin-relevanten Aktionen sind auditierbar. **Test:** Zweiter Bootstrap und ungültige Einladung werden verweigert.

## Persona C – Designer im Team

**Aktionen:** Projekt finden, Datei herunterladen, lokal bearbeiten, mit bekannter Basisversion neue Revision hochladen, ältere Revision ansehen/wiederherstellen. **Erwartung:** Kein stilles Überschreiben paralleler Änderungen. **Test:** Zwei Clients speichern von derselben Version; einer erhält 412 `VERSION_CONFLICT` (siehe ADR-0009) und einen Konfliktdialog.

## Persona D – Externer Reviewer

**Aktionen:** Nur in bestimmtem Projekt lesen und kommentieren (Kommentare später). **Erwartung:** Keine Mitgliederliste, keine anderen Projekte, keine Admin-Metadaten. **Test:** Token/Link auf fremde IDs austauschen, Zugriff verweigert ohne Existenzleak.

## Persona E – Integrationsentwickler

**Aktionen:** Discovery (`/api/v1/info`), Desktop-Login, Liste der Workspaces, Upload an temporäres Ziel, Commit einer neuen Version. **Erwartung:** Typisierter SDK-Vertrag, Retry/Idempotenz, stabile Fehlertypen, Version-Negotiation und Local-first-Fallback. **Test:** Unterbrochene Verbindung/Token-Ablauf; keine Datenverluste.
