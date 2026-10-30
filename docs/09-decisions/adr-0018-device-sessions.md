# ADR-0018: Desktop-Anmeldung und Geräte-Sitzungen

- Status: Accepted
- Datum: 2026-10-30
- Bezug: [AC-01](../12-app-cloud/ac-01-desktop-sign-in.md), [ADR-0009](adr-0009-conflict-status-and-auth-schemes.md)

## Kontext

Craft-Apps, Launcher und CLI brauchen eine eigene Anmeldung an einer CraftCloud-Instanz. Die Browser-Anmeldung mit Cookie bleibt erhalten. Jeder Client meldet sich selbst an; der Launcher darf keine Tokens an Apps weitergeben.

## Entscheidung

1. Geräte-Sitzungen liegen getrennt von Browser-Sitzungen in `device_sessions` (Migration `0015_device_sessions.sql`). Sie tragen Benutzer, bereinigten Gerätenamen mit höchstens 64 Zeichen, Art (`app`, `launcher`, `cli`), App-ID nach `^[a-z0-9-]{1,32}$`, Token-Hashes, eine UUID-Familie, Zeitstempel und Widerrufsgrund.
2. Zugriffstokens (`ccd_at_`) und Erneuerungstokens (`ccd_rt_`) enthalten mindestens 256 zufällige Bit. Gespeichert werden ausschließlich SHA-256-Hashes. JWT wird nicht verwendet.
3. Zugriffstokens gelten standardmäßig 15 Minuten. Geräte-Sitzungen enden nach 30 Tagen Leerlauf oder spätestens 90 Tagen. `CRAFTCLOUD_DEVICE_ACCESS_TTL`, `CRAFTCLOUD_DEVICE_IDLE_DAYS` und `CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS` erlauben validierte Anpassungen.
4. Jede Erneuerung ersetzt beide Tokens atomar innerhalb derselben Familie. Bereits benutzte Refresh-Hashes bleiben zur Wiederverwendungserkennung erhalten. Eine Wiederverwendung widerruft die gesamte Familie mit `refresh_reuse` und einem Audit-Ereignis. Die optionale Zehn-Sekunden-Gnadenfrist wird nicht genutzt: Auch ein sofortiger doppelter Versuch widerruft die Familie. Damit sind weder Klartext-Tokens noch ein verschlüsselter Antwortcache erforderlich. Clients müssen Refresh-Aufrufe serialisieren; nach einer verlorenen Antwort ist gegebenenfalls eine neue Anmeldung nötig.
5. `POST /auth/device/login`, `/auth/device/refresh` und `/auth/device/logout` sowie `GET /auth/devices` und `DELETE /auth/devices/{id}` ergänzen `/api/v1`. Login verwendet die bestehende Passwortprüfung, das Verhalten für unbekannte E-Mail-Adressen und gemeinsame Ratenbegrenzungen. AC-01 enthält nur eine Erweiterungsstelle für die Zweifaktor-Anmeldung; AC-02 ergänzt Challenge und zweiten Schritt.
6. Die zentrale Actor-Ermittlung akzeptiert Bearer-Zugriffstokens. Nur Bearer-Anfragen benötigen keinen CSRF-Nachweis und keine Origin-Prüfung. Cookie und Bearer zusammen ergeben `400 AMBIGUOUS_CREDENTIALS`. Tokens aus Query-Parametern werden nicht akzeptiert. Die Policy wird für jede Anfrage unverändert ausgewertet.
7. Passwortänderung, Passwort-Reset, Konto-Deaktivierung, „Überall abmelden“ und der Verlust der letzten Organisationsmitgliedschaft widerrufen Geräte-Sitzungen in derselben Transaktion. Der Upload-Abschluss prüft Geräte-Sitzungen unter dem Organisations-Lock erneut; die Sperrreihenfolge bleibt Organisation, Benutzer, Sitzung.
8. Geräte-POST-Routen verlangen TLS, außer bei einer expliziten Loopback-Instanz. Proxy-Angaben werden nur im vorhandenen Vertrauensmodell akzeptiert. Der Client speichert das Erneuerungstoken künftig im Betriebssystem-Schlüsselbund (AC-04), niemals in einer Einstellungsdatei.
9. Audit-Ereignisse heißen `auth.device_login`, `auth.device_refresh_reuse`, `auth.device_logout` und `auth.device_revoked`. Ziele sind Geräte-Sitzungs-IDs; Tokens, Passwörter, Gerätenamen und E-Mail-Adressen gehören weder in Audit noch Logs.
10. Das Webpanel listet eigene Geräte in den Einstellungen und erlaubt deren Abmeldung. Die Texte sind auf Englisch und Deutsch verfügbar; Komponententests und axe prüfen beide Sprachen. Fremde Geräte dürfen auch Operatoren und Organisationsadministratoren in AC-01 nicht verwalten.
11. Der Wartungsjob entfernt seit mindestens 30 Tagen abgelaufene oder widerrufene Geräte-Sitzungen einschließlich ihrer Refresh-Historie. Geräte-Sitzungen und Refresh-Historie werden ebenso wie Browser-Sitzungen nicht gesichert und nach einem Restore nicht wiederbelebt.

## Alternativen und Folgen

Ein Browser-Device-Flow und OIDC bleiben spätere Erweiterungen. Die ausdrückliche Produktentscheidung erlaubt hier E-Mail und Passwort über TLS. Opaque Tokens erlauben sofortigen serverseitigen Widerruf ohne JWT-Schlüsselverwaltung. Der Datenbankzugriff je Anfrage und die Refresh-Historie verursachen zusätzliche Last; Laufzeitgrenzen, Ratenbegrenzungen und Wartung begrenzen diese Angriffsfläche. Ein gestohlenes noch gültiges Zugriffstoken bleibt bis zu Ablauf oder Widerruf verwendbar. TLS, Betriebssystem-Schlüsselbund und Geheimnisfreiheit der Logs bleiben notwendige Schutzmaßnahmen.

Die Änderung ist additiv zur Cookie-Anmeldung und ersetzt die frühere pauschale Zurückstellung von Desktop-Passwortanmeldungen. Ein unabhängiges Sicherheitsreview und die im Briefing genannten Prüfungen bleiben Abnahmevoraussetzungen.
