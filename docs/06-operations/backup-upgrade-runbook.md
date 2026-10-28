# Backup, Restore, Upgrades – Betreiber-Runbook als Entwurf

## Was gesichert werden muss

PostgreSQL-Metadaten (Users, orgs, ACL, file versions, blob map, audit according to retention), Blobdaten auf Storage-Backend, Config-Version und versionskompatible Key-Secrets. Datenbanken enthalten Pointer, die ohne Blobbestand unbrauchbar sind. Eine bloße Kopie des lokalen Upload-Ordners ist **kein** vollständiges Backup.

## Backup-Algorithmus (zu implementieren)

1. Konsistenten Snapshot-/Checkpoint definieren: laufende Uploads markieren, committed version refs fixieren, GC-Freeze/Lease.
2. Datenbanksnapshot mit WAL/consistent transaction exportieren.
3. Blobmanifest aus committed revisions bilden; Objektmetadaten und SHA-256 erfassen.
4. Dateiblobs kopieren/sichern, Checksummen nachverifizieren; uncommitted uploads nicht als committed listen.
5. Manifest + DB schema version + service version + Zeitpunkt speichern; Backup verschlüsseln, extern aufbewahren.
6. Alle Phasen/Fehler auditieren, keinen Teilerfolg als vollständiges Backup melden.

## Restore-Algorithmus

1. Service in Wartungsmodus, Input-Backup und Secrets validieren.
2. Auf isolierter neuer Instanz Schema/Version prüfen; DB und Blobbestand importieren.
3. Alle referenzierten Objektchecksummen verifizieren; dangling refs melden/abbrechen.
4. Tenant-IDs, Version Heads, ACL, Audit und aktuellen Bootstrapzustand prüfen.
5. Proxy/Domain/Email settings neu prüfen und Refresh-/Session-Revoke-Strategie bestimmen.
6. Teste zufällige Dateien aus mehreren Workspaces, Upload einer neuen Version, 403 cross-tenant.
7. Erst dann Produktivrouting umstellen, ansonsten Rollback zur gesicherten alten Instanz.

## Upgrade-Check

Schema-Migrationen sind nur vorwärts gerichtet (kein Down-Skript): vor jedem `craftcloud-server migrate` ein Datenbank-Backup erstellen; das Zurückspielen dieses Backups ist der Rollback-Pfad (ADR-0012).

Vorher Vollbackup und Restore-Drill, Release Notes/DB migration compatibility, Client/API matrix. Währenddessen Health/Readiness und Upload-Drain. Nachher Smoke Tests für Login, Project list, Upload/Download, version conflict, backup age. Keine stillen Downgrades über nicht rückwärtskompatible Migrationen.

## RPO/RTO

Werte sind Betreiberziele und müssen **gemessen** werden. Vorschlag Pilot: 24 h RPO und 4 h RTO nur nach bestandenem Test, kein Garantieversprechen.
