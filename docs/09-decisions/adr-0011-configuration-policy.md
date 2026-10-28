# ADR-0011: Konfigurationsrichtlinie (Umgebung, Secret-Dateien, vertrauenswürdige Proxys)

- **Status:** Proposed (unabhängiger lokaler Prototyp; bei Übergabe an Maintainer erneut zu prüfen)
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps

## Kontext

PR-002 (CC-P01-02) führt Konfiguration, Secret-Dateien, Proxy-Vertrauen und einen begrenzten Shutdown ein. Fehlkonfiguration darf weder still ignoriert werden noch den Server in einen unsicheren Zustand bringen; `layout-and-boundaries.md` verlangt, dass Bibliotheks-Crates keine Umgebungsvariablen lesen.

## Entscheidung

1. **Nur Umgebung.** Konfiguration kommt ausschließlich aus Umgebungsvariablen mit Präfix `CRAFTCLOUD_`, gelesen und validiert im Server-Binary (`apps/craftcloud-server`). Die Validierung ist eine reine Funktion über eine Key/Value-Map; alle Fehler werden gesammelt und mit Schlüsselnamen gemeldet.
2. **Unbekannte Schlüssel sind Fehler** (Tippfehlerschutz, auch bei abweichender Groß-/Kleinschreibung des Präfixes). Für spätere Meilensteine reservierte Schlüssel (`DATABASE_URL`, `STORAGE_BACKEND`, `STORAGE_ROOT`, `BOOTSTRAP_SECRET_FILE`, `SESSION_SECRET_FILE`) werden ausdrücklich mit „not supported in this build yet“ abgelehnt statt ignoriert.
3. **Ungültige Konfiguration** beendet den Prozess mit **Exit-Code 2**, ohne einen Socket zu binden und ohne Panic; Laufzeitfehler liefern Exit-Code 1.
4. **`PUBLIC_URL`:** absolute URL mit Schema `https`; `http` nur für Loopback-Hosts (`localhost`, `127.0.0.0/8`, `::1`). Keine Userinfo, kein Query, kein Fragment; ein abschließender Schrägstrich wird normalisiert. Fehlermeldungen geben den Wert nie aus. Absolute URLs werden nur aus diesem Wert gebildet, nie aus `Host` oder `X-Forwarded-*`.
5. **`TRUSTED_PROXIES`:** Liste von IPs/CIDRs, Standard leer (niemandem wird vertraut). `0.0.0.0/0` und `::/0` werden abgelehnt, weil sonst jeder Client `X-Forwarded-For` fälschen könnte. `X-Forwarded-For` wird nur ausgewertet, wenn die Peer-Adresse vertrauenswürdig ist; dann von rechts nach links über vertrauenswürdige Hops hinweg bis zur ersten nicht vertrauenswürdigen Adresse (sind alle Hops vertrauenswürdig, gilt der linkeste). Fehlerhafte oder übergroße Header (über 1 KiB oder mehr als 16 Hops) fallen auf die Peer-Adresse zurück. Die Client-Adresse wird standardmäßig nicht geloggt.
6. **Limits mit festen Bereichen:** `MAX_BODY_BYTES` 1024 bis 16 MiB (Standard 64 KiB), `REQUEST_TIMEOUT_SECS` 1 bis 300 (10), `SHUTDOWN_GRACE_SECS` 1 bis 120 (10); geprüftes Parsen ohne Überlauf.
7. **Secret-Dateien** (Loader vorhanden, noch an keinen Schlüssel angeschlossen): absoluter Pfad, reguläre Datei (kein Symlink, kein Verzeichnis), höchstens 4 KiB, gültiges UTF-8, nach Entfernen eines abschließenden Zeilenumbruchs nicht leer, unter Unix nicht für Gruppe/andere lesbar (unter Windows keine Rechteprüfung). Der Wert wird nie ausgegeben (`[redacted]`) und beim Freigeben im Speicher überschrieben (`zeroize`).
8. **Fehlerformat für Limits:** 413 mit Code `PAYLOAD_TOO_LARGE`; Handler-Zeitüberschreitung als 503 mit Code `TIMEOUT` (408 bleibt langsamen Clients vorbehalten); beide im JSON-Fehlerformat mit Request-ID.

## Konsequenzen und Risiken

Neue Abhängigkeiten: `url`, `ipnet`, `zeroize`. Strikte Ablehnung unbekannter Schlüssel kann Deployments brechen, die zusätzliche `CRAFTCLOUD_*`-Variablen setzen. Die Lücke zwischen Prüfung und Lesen einer Secret-Datei (TOCTOU) wird nur durch erneute Prüfung am geöffneten Handle verkleinert. Der Grace-Timeout bricht Verbindungen nicht aktiv ab; der Prozess beendet sich danach.

## Betrachtete Alternativen

Konfigurationsdatei (TOML) zusätzlich zur Umgebung (mehr Oberfläche, später möglich); unbekannte Schlüssel nur warnen (Tippfehler bleiben unbemerkt); `X-Forwarded-*` immer vertrauen (Spoofing).

## Review und Revision

Zusammen mit ADR-0010 vor Übergabe an Maintainer prüfen. Reservierte Schlüssel werden mit dem jeweiligen Meilenstein (DB, Storage, Bootstrap, Sessions) freigeschaltet.

## Nachtrag 2026-10-28

Mit PR-003 und PR-004 sind `CRAFTCLOUD_DATABASE_URL_FILE` und `CRAFTCLOUD_BOOTSTRAP_SECRET_FILE` keine reservierten Schlüssel mehr, sondern unterstützt. `CRAFTCLOUD_DATABASE_URL` bleibt abgelehnt (nur über Datei). Maßgeblich für die aktuelle Schlüsselliste ist `docs/06-operations/deployment.md`.
