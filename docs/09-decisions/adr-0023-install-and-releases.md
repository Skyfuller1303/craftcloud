# ADR-0023: Quellreleases und Installation auf eigenen Linux-Servern

- Status: Für OPS-01 gemäß Entscheidung des Projektverantwortlichen vom 2026-10-30 umzusetzen; Abnahme und externe Betriebsprüfung stehen aus.
- Datum: 2026-10-30

## Entscheidung

CraftCloud wird als versioniertes Quellarchiv mit SHA-256-Prüfsummen und CycloneDX-SBOM verteilt. Das Installationsskript baut das Container-Image lokal; vorgebaute Images, Registry-Veröffentlichung und automatische Updates sind nicht Bestandteil dieser Entscheidung. SemVer beginnt bei 0.1.0; Tags und Veröffentlichungen benötigen einen eigenen Auftrag. Eine minisign-Signatur ist vorgesehen, wird ohne Projektschlüssel aber nicht erzeugt.

Ziel sind Linux x86_64 und arm64 mit Docker Engine ab 25 und Compose ab 2.24. Debian 13 und Ubuntu 24.04 LTS sind die vorgesehenen offiziellen Testplattformen; ein Docker-Desktop-Test ersetzt deren Prüfung nicht. Programmstände liegen unter /opt/craftcloud/releases, der aktive Stand unter /opt/craftcloud/current, Konfiguration unter /etc/craftcloud und Daten unter /var/lib/craftcloud.

Installation und Steuerung erfolgen mit POSIX-sh-Skripten. Geheimnisse werden über den bestehenden secrets-init-Dienst als Dateien bereitgestellt, niemals ausgegeben oder bei einer Wiederholung ersetzt. Upgrades benötigen vor jeder Migration ein erfolgreiches Backup. Ein Rückwechsel des Programms ersetzt keinen Datenbank-Restore; nach Migrationen erfolgt ein Restore ausschließlich nach bewusster Betreiberentscheidung anhand des Runbooks.

systemd startet den Stack und plant täglich um 03:15 mit bis zu 30 Minuten Verzögerung ein Backup. Standardmäßig bleiben sieben erfolgreiche Backups erhalten. Verschlüsselte Kopien außer Haus und regelmäßige Restore-Proben verantwortet der Betreiber. SMTP, Windows Server, Hochverfügbarkeit und automatische Restores bleiben außerhalb von OPS-01.

Review-Korrekturen: Archive werden zuerst in ein Staging-Verzeichnis entpackt, geprüft und atomar umbenannt, damit ein fehlgeschlagener Lauf wiederholbar bleibt. Das Archiv ist reproduzierbar (feste gzip-Kopfzeit, normalisierte Eigentümer). Der Schlüssel der Zwei-Faktor-Anmeldung wird einmalig erzeugt, nie überschrieben und ist bewusst nicht Teil des Datenbank-Backups. `craftcloud-ctl` serialisiert Backup, Upgrade und Rollback mit `flock`. Beim Upgrade werden unveränderte mitgelieferte Caddy-Dateien erneuert; Betreiberänderungen bleiben, die neue Fassung liegt als `.new` daneben.

## Alternativen und Folgen

Vorgebaute Registry-Images würden die Installation beschleunigen, benötigen aber eine zusätzliche Veröffentlichungs- und Lieferkettenentscheidung. Debian-Pakete und Kubernetes erhöhen den Betriebsumfang und werden hier nicht eingeführt. Lokale Builds benötigen Netzwerkzugriff, Arbeitsspeicher und Zeit; Prüfsummen schützen ohne getrennt vertrauenswürdigen Bezug nicht gegen einen ausgetauschten Download. Echte VM- und Architekturprüfungen bleiben Voraussetzungen für eine belastbare Betriebsfreigabe.

## Nachweise

[OPS-01-Briefing](../12-app-cloud/ops-01-install-and-releases.md), [Installationsanleitung](../06-operations/install.md) und der lokale OPS-01-Bericht dokumentieren Umsetzung, Prüfungen und verbleibende Einschränkungen.

## Nachtrag 2026-10-31: FigCraft-Begleitdienst standardmäßig

Der Installer richtet den FigCraft-Begleitdienst aus [ADR-0022](adr-0022-figcraft-live-hosting.md) standardmäßig ein (`CRAFTCLOUD_FIGCRAFT=enabled`, Opt-out mit `--no-figcraft` oder `craftcloud-ctl figcraft disable`). Eine einzige Hilfsfunktion legt die Compose-Dateiliste fest, damit `install.sh` und `craftcloud-ctl` nicht auseinanderlaufen. Der Verwaltungsschlüssel liegt als `/etc/craftcloud/figcraft.key` (0600) und ist nicht im Backup.
