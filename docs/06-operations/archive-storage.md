# Externes Versionsarchiv (AC-12)

**Entwicklungsstand:** Die Archivierung ist noch nicht zur Aktivierung freigegeben. Vor produktiver Nutzung müssen Serververdrahtung, Rückholung, Aufbewahrung, Backup und unabhängiger Sicherheitsreview vollständig geprüft sein; maßgeblich ist der AC-12-Bericht. Die Beispiele beschreiben die beabsichtigte Konfiguration, keine geprüfte Installation bei einem bestimmten Anbieter.

Die aktuelle Version jeder Datei und laufende Uploads liegen lokal. Ältere Versionen können nach einer Frist extern gespeichert werden. Der Anbieter sieht opake Schlüssel und verschlüsselte Bytes. Die vollständige Dateilänge und Zugriffsmuster können trotzdem sichtbar sein. Das Archiv ist kein Ersatz für unabhängige Sicherungen.

## S3-kompatiblen Speicher vorbereiten

Legen Sie einen privaten Bucket und bei Bedarf einen eigenen, ausschließlich CraftCloud zugeordneten Präfix an. Sperren Sie öffentlichen Zugriff und verwenden Sie TLS mit gültigem Zertifikat. Erstellen Sie eigene Zugangsschlüssel mit Rechten nur für diesen Bucket und Präfix: Objekte auflisten, lesen, schreiben, löschen und Multipart-Uploads erstellen, auflisten und abbrechen. Prüfen Sie mit einem Negativtest, dass kein Zugriff auf andere Buckets besteht. Aktivieren Sie Bucket-Versionierung oder Object Lock und berücksichtigen Sie deren Aufbewahrung und Kosten.

Die Provider-Einstellungen heißen `CRAFTCLOUD_ARCHIVE_STORE=s3`, `CRAFTCLOUD_ARCHIVE_S3_ENDPOINT`, `CRAFTCLOUD_ARCHIVE_S3_REGION`, `CRAFTCLOUD_ARCHIVE_S3_BUCKET`, optional `CRAFTCLOUD_ARCHIVE_S3_PREFIX`, `CRAFTCLOUD_ARCHIVE_S3_ACCESS_KEY_ID_FILE` und `CRAFTCLOUD_ARCHIVE_S3_SECRET_ACCESS_KEY_FILE`. Die beiden letzten Werte sind absolute Pfade auf geschützte Dateien, nicht die Zugangsschlüssel selbst. Stellen Sie außerdem den vorhandenen Serverschlüssel über `CRAFTCLOUD_SECRET_KEY_FILE` bereit. Die Ausnahme für unverschlüsseltes HTTP gilt nur für Loopback-Tests.

## Hetzner Storage Box über SFTP vorbereiten

Legen Sie ein eigenes Verzeichnis und einen SSH-Schlüssel mit möglichst eingeschränkten Rechten an. Für die Storage Box wird üblicherweise Port 23 verwendet; kontrollieren Sie die aktuellen Anbieterangaben für Ihr Konto. Ermitteln Sie den Host-Key und vergleichen Sie seinen Fingerabdruck über einen unabhängigen, vertrauenswürdigen Kanal mit dem vom Anbieter veröffentlichten Wert. Übernehmen Sie erst nach diesem Vergleich die passende Zeile in eine separate `known_hosts`-Datei; eine ungeprüfte Ausgabe von `ssh-keyscan` allein ist kein Vertrauensnachweis. Hinterlegen Sie den privaten SSH-Schlüssel und die `known_hosts`-Datei geschützt auf dem Server. Ein unbekannter oder geänderter Host-Key muss den Verbindungsaufbau abbrechen.

Konfigurieren Sie `CRAFTCLOUD_ARCHIVE_STORE=sftp`, `CRAFTCLOUD_ARCHIVE_SFTP_HOST`, `CRAFTCLOUD_ARCHIVE_SFTP_PORT`, `CRAFTCLOUD_ARCHIVE_SFTP_USER`, `CRAFTCLOUD_ARCHIVE_SFTP_BASE_DIRECTORY`, `CRAFTCLOUD_ARCHIVE_SFTP_PRIVATE_KEY_FILE` und `CRAFTCLOUD_ARCHIVE_SFTP_KNOWN_HOSTS_FILE`. Verwenden Sie Schlüsseldateien statt Passwortanmeldung und aktivieren Sie beim Anbieter Snapshots. Die parallelen Verbindungen sind begrenzt. SMB, sshfs und andere eingebundene Netzlaufwerke ersetzen die Archivstufe nicht: Sie trennen die Speicherzustände nicht und ein Ausfall kann lokale Serveroperationen blockieren.

Zum Beispiel kann `ssh-keyscan -p 23 <host> > known_hosts.candidate` einen Kandidaten erfassen und `ssh-keygen -lf known_hosts.candidate` seinen Fingerabdruck anzeigen. Diese Ausgabe ist erst nach unabhängigem Vergleich vertrauenswürdig; übernehmen Sie anschließend ausschließlich die geprüfte Zeile in die konfigurierte Datei.

## Frist, Rückholung und Wechsel

`CRAFTCLOUD_ARCHIVE_AFTER` bestimmt die Wartezeit nach Ablösung einer Version; der Standard ist `24h`, zulässig sind `0` bis `90d`. `CRAFTCLOUD_ARCHIVE_MAX_MBPS` setzt ein Limit in dezimalen Mbit/s für Provider-Transfer und Readback. S3 kann dabei durch einen einzelnen Request-Body bis zu 8 MiB kurzzeitig darüber liegen. Vor Deaktivierung oder Anbieterwechsel müssen sämtliche Archivobjekte auf den lokalen Speicher zurückgeholt und gegen Größe und SHA-256 geprüft werden. Planen Sie ausreichend freien Platz ein; bei unbekanntem oder unzureichendem Platz muss die Rückholung abbrechen. Ein direkter Anbieterwechsel ohne Rückholung ist nicht Bestandteil von AC-12.

Der Folgeauftrag ergänzt Audit-Ereignisse für Konfigurationswechsel, Operator-Rückholung und die Freigabe von Backup-Pins. Diese Erweiterung muss zusammen mit dem endgültigen Serverstart und unabhängigen Sicherheitsprüfungen abgenommen werden. Bis zur Freigabe müssen Betreiber Änderungen an der Archivkonfiguration außerhalb des Dienstes nachvollziehbar protokollieren.

## Budgetierte Archivprüfung

Die vorbereitete Archivprüfung authentifiziert externe Daten blockweise und kann bei großen Objekten über mehrere Wartungsläufe fortgesetzt werden. `CRAFTCLOUD_ARCHIVE_VERIFY_BYTES_PER_RUN` begrenzt die pro Lauf geprüften Klartextbytes auf 65.536 bis 1.073.741.824; der Standard beträgt 4.194.304 Bytes. `CRAFTCLOUD_ARCHIVE_VERIFY_SECONDS_PER_RUN` begrenzt die Prüfzeit auf 1 bis 30 Sekunden; der Standard beträgt 5 Sekunden. Der Fortschritt und der kontrollierte Fehlerstatus liegen in der Datenbank. Bei einem langsamen Anbieter kann ein größeres Zeitbudget nötig sein, damit mindestens ein vollständiger Block geprüft wird. Die Aktivierung bleibt bis zur vollständigen Prüfung des Folgeauftrags gesperrt.

Die vorgesehenen Befehle sind `craftcloud-server archive status` und `craftcloud-server archive recall` beziehungsweise `craftcloud-server archive recall --org <uuid>`. Die alte Anbieter-Konfiguration und der passende Serverschlüssel müssen während der Rückholung verfügbar bleiben. Erst nach vollständig erfolgreicher Rückholung und Statusprüfung darf der Anbieter gewechselt oder die Archivkonfiguration entfernt werden. Diese Befehle benötigen vor Freigabe noch einen dokumentierten End-to-End-Test.

## Sicherung und Notfall-Restore

Bewahren Sie Datenbankbackup und zugehörige Versionen oder Snapshots des Archivanbieters passend zum Backupzeitpunkt auf. Sichern Sie den Serverschlüssel getrennt und geschützt: Ohne denselben Schlüssel lassen sich verpackte Datenschlüssel und damit Archivobjekte nicht entschlüsseln. Eine neue Schlüsseldatei ersetzt ihn nicht. Prüfen Sie Restore regelmäßig auf einer isolierten Instanz mit passenden Providerrechten; vor Wiederaufnahme der Archiv-GC müssen sämtliche noch aufbewahrten Backupkopien inventarisiert werden. Erst eine ausdrückliche Bestätigung einer vollständigen Inventur darf die Archiv-GC freigeben.

Das bestehende [Backup- und Upgrade-Runbook](backup-upgrade-runbook.md) beschreibt den lokalen Restore. Für das externe Archiv werden die exakten, getesteten Inventur-, Verify-, Recall- und Freigabebefehle ergänzt, bevor die Funktion als betriebsbereit gilt. Löschen Sie keine Archivobjekte aufgrund einer bloßen Sicht auf die aktuelle Datenbank: Ältere Backups können sie weiterhin benötigen.
