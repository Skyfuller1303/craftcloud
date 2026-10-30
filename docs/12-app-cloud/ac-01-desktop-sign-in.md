# AC-01 — Desktop-Anmeldung und Geräte-Sitzungen

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac01`, Branch `feat/ac-01-desktop-sign-in` (legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-01\REPORT.md`. **Migration:** `0015_device_sessions.sql` (Nummer reserviert). **Review:** unabhängig, sicherheitsrelevant.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac01 (Branch feat/ac-01-desktop-sign-in).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-01-desktop-sign-in.md vollständig
und setze AC-01 genau danach um. Halte die Arbeitsweise aus dem README ein (speicherbegrenzte
Builds, keine Commits/Pushes, keine neuen Abhängigkeiten ohne Begründung).
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-01\REPORT.md im vorgegebenen Format.
```

## Ziel

Desktop-Clients (Craft-Apps, Launcher, Kommandozeile) melden sich mit E-Mail und Passwort an und erhalten eine **Geräte-Sitzung**: ein kurzlebiges Zugriffstoken für API-Aufrufe und ein rotierendes Erneuerungstoken für den nächsten Start. Die bestehende Cookie-Sitzung im Browser bleibt unverändert. Nutzer sehen und beenden ihre Geräte im Webpanel.

## Ist-Stand

- Cookie-Sitzungen: Tabelle `sessions` (`0004_sessions_and_password_reset.sql`), Logik in `crates/craftcloud-api/src/session.rs` und `auth.rs`, opake Tokens in `crates/craftcloud-auth/src/opaque.rs`, Passwortprüfung `password.rs` (Argon2id), Login-Ratenbegrenzung `ratelimit.rs`, CSRF/Origin für Cookie-Anfragen.
- Routen in `contracts/openapi.yaml`: `/auth/login`, `/auth/session`, `/auth/sessions`, `/auth/logout`, `/auth/logout-all`, `/auth/password/change`, `/auth/password-reset`. Kein Bearer-Token. ADR-0009 hält fest, dass ein Bearer-Schema später additiv kommt.
- Sitzungsdauer Browser: 8 h Leerlauf, 7 d absolut (Entscheidung P1).
- Sitzungsprüfung beim Upload-Abschluss unter dem Organisations-Lock (`OrgTx::session_valid`, PR-020).

## Entscheidungen (fest)

1. **Neue Tabelle `device_sessions`**, getrennt von `sessions`: `id`, `user_id`, `name` (vom Client, bereinigt, max. 64 Zeichen, z. B. „PhotoCraft auf LAPTOP-7“), `client_kind` (`app`, `launcher`, `cli`), `client_id` (`^[a-z0-9-]{1,32}$`, z. B. `photocraft`), `refresh_hash` (32 Byte), `refresh_family` (uuid), `access_hash` (32 Byte, nullable), `access_expires_at`, `created_at`, `last_used_at`, `idle_expires_at`, `absolute_expires_at`, `revoked_at`, `revoke_reason`.
2. **Tokens:** beide opak (bestehende Opaque-Token-Hilfen, mindestens 256 Bit), nur als SHA-256 gespeichert, mit Präfix zur Erkennung in Logs/Scannern (`ccd_at_` Zugriff, `ccd_rt_` Erneuerung). Kein JWT.
3. **Laufzeiten:** Zugriffstoken 15 min. Erneuerungstoken 30 Tage Leerlauf, 90 Tage absolut. Werte als Konfiguration mit diesen Standardwerten (`CRAFTCLOUD_DEVICE_ACCESS_TTL`, `CRAFTCLOUD_DEVICE_IDLE_DAYS`, `CRAFTCLOUD_DEVICE_ABSOLUTE_DAYS`), Grenzen in `config.rs` validiert.
4. **Rotation mit Wiederverwendungserkennung:** Jede Erneuerung erzeugt ein neues Erneuerungstoken derselben Familie und macht das alte ungültig. Wird ein bereits rotiertes Erneuerungstoken erneut vorgelegt, wird die ganze Familie widerrufen (`revoke_reason = refresh_reuse`) und ein Audit-Ereignis geschrieben. Eine kurze Gnadenfrist (10 s) für das unmittelbar vorherige Token ist erlaubt, damit ein doppelter Netzwerkversuch nicht abmeldet; sie liefert dasselbe Ergebnis wie die erste Erneuerung (idempotent über die Familie), kein drittes Token.
5. **Routen** (alle unter `/api/v1`, OpenAPI ergänzen):
   - `POST /auth/device/login` — Body `{email, password, client_kind, client_id, device_name}`; Antwort `{access_token, access_expires_at, refresh_token, refresh_expires_at, device_session_id, user}`. Gleiche Ratenbegrenzung und gleiches Zeitverhalten für unbekannte E-Mail wie `/auth/login`. Ist 2FA aktiv (AC-02), antwortet die Route mit 401 `SECOND_FACTOR_REQUIRED` und einem kurzlebigen `challenge_id`; AC-02 ergänzt den zweiten Schritt. In AC-01 nur die Erweiterungsstelle vorsehen (Enum/Trait), nicht implementieren.
   - `POST /auth/device/refresh` — Body `{refresh_token}`; Antwort wie Login ohne `user`.
   - `POST /auth/device/logout` — mit Bearer; widerruft die eigene Geräte-Sitzung.
   - `GET /auth/devices` — Liste der eigenen Geräte-Sitzungen (Cookie oder Bearer). `DELETE /auth/devices/{id}` — eigenes Gerät beenden.
   - Operator/Org-Admin: keine fremden Geräte in AC-01 (Folgearbeit).
6. **Bearer-Authentifizierung:** `Authorization: Bearer ccd_at_…` wird von derselben Actor-Ermittlung akzeptiert wie das Cookie. Für Bearer-Anfragen entfallen CSRF-Token und Origin-Prüfung (kein ambienter Nachweis). Eine Anfrage mit Cookie **und** Bearer wird mit 400 `AMBIGUOUS_CREDENTIALS` abgelehnt. Bearer-Tokens in der Query werden nie akzeptiert.
7. **Ungültig machen:** Passwortänderung, Passwort-Reset, Deaktivierung des Kontos, „Überall abmelden“ und Entfernen aus der letzten Organisation widerrufen alle Geräte-Sitzungen des Kontos in derselben Transaktion. `OrgTx::session_valid` prüft Geräte-Sitzungen gleichwertig zu Cookie-Sitzungen (Sperrreihenfolge Organisation, Benutzer, Sitzung bleibt).
8. **TLS-Pflicht:** `POST /auth/device/*` verweigert Klartext-HTTP, außer `CRAFTCLOUD_PUBLIC_URL` ist Loopback (wie die Cookie-Policy). Bei Betrieb hinter Proxy gilt die vorhandene Trusted-Proxy-Logik.
9. **Audit:** `auth.device_login`, `auth.device_refresh_reuse`, `auth.device_logout`, `auth.device_revoked` (Ziel = Geräte-Sitzung, nie Token).
10. **Webpanel:** In den Einstellungen eine Liste „Geräte“ (Name, Art, App, zuletzt verwendet, angelegt) mit „Abmelden“ je Zeile; Texte in `en.tsv` und `de.tsv`; axe-Tests EN/DE; Design nach `docs/11-design/`.
11. **Aufräumen:** Der Wartungsjob löscht abgelaufene und widerrufene Geräte-Sitzungen nach 30 Tagen (wie Sitzungen).

## Erlaubte Dateien

- `crates/craftcloud-auth/src/**`, `crates/craftcloud-api/src/{auth,session,lib,maintenance}.rs` und neue Module `crates/craftcloud-api/src/device.rs`, `crates/craftcloud-db/src/**` (neues Modul `device_sessions.rs`), `crates/craftcloud-db/migrations/0015_device_sessions.sql`, `crates/craftcloud-core/src/policy.rs` (nur falls neue Aktionen nötig), `apps/craftcloud-server/src/config.rs`.
- `crates/craftcloud-backup/src/tables.rs`: `device_sessions` wird wie `sessions` **nicht** gesichert (in die Ausschlussliste, Test anpassen).
- `contracts/openapi.yaml` und der OpenAPI-Snapshot (`cargo xtask openapi-snapshot`).
- `apps/craftcloud-admin/src/**` nur für die Geräteliste in den Einstellungen.
- Doku: `docs/03-features/cc-f02-identity-and-sessions.md` (Status), `docs/05-security/threat-model.md` (Bearer-Tokens), `docs/06-operations/deployment.md` (neue Variablen), neues ADR `docs/09-decisions/adr-0018-device-sessions.md`, `docs/gaps.md`.
- Nicht ändern: `docs/09-decisions/decision-log.md` (Entscheidungen in den Bericht), andere Briefings.

## Tests (Pflicht)

- Unit: Token-Format, Hashing, Laufzeitberechnung, Konfigurationsgrenzen.
- `cargo xtask db-test`: Login richtig/falsch/unbekannt (gleiche Antwort und Ratenbegrenzung), Refresh-Rotation, Wiederverwendung widerruft Familie, Gnadenfrist liefert kein drittes Token, Ablauf Leerlauf/absolut, Bearer auf jeder bestehenden geschützten Route akzeptiert (Stichprobe je Modul) und mit falscher Org 404 wie Cookie, Cookie+Bearer 400, Bearer ohne CSRF erlaubt, Cookie ohne CSRF weiterhin 403, Passwortänderung/Reset/Deaktivierung widerruft Geräte, Upload-Abschluss mit widerrufenem Gerät 401 (wie SR-02), Log-Test: kein Token, keine E-Mail im Log (bestehende Testschicht erweitern).
- Web: Komponententest Geräteliste, axe EN/DE.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt, Exitcodes im Bericht).
2. Alle Entscheidungen 1–11 umgesetzt oder im Bericht begründet abweichend.
3. OpenAPI enthält die neuen Routen, das Bearer-Schema und die Fehlercodes `SECOND_FACTOR_REQUIRED`, `AMBIGUOUS_CREDENTIALS`, `REFRESH_TOKEN_INVALID`.
4. Kein Token, Passwort oder E-Mail im Log oder Audit (Test).
5. ADR-0018 angelegt (Status „Accepted“, Datum 2026-10-30, Entscheidung wie oben).
6. Bestehende Browser-Anmeldung unverändert (alle bestehenden Tests grün).

## Nicht Teil von AC-01

Zweiter Faktor (AC-02), Gerätefreigabe per Browser (Device Authorization Grant, RFC 8628) und OIDC, fremde Geräte durch Admins beenden, Schlüsselbund-Code in Clients (AC-04).
