# AC-02 — Zwei-Faktor-Anmeldung (TOTP)

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac02`, Branch `feat/ac-02-two-factor` (Basis: `main` nach Merge von AC-01). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-02\REPORT.md`. **Migration:** `0017_two_factor.sql`. **Review:** unabhängig, sicherheitsrelevant.

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac02 (Branch feat/ac-02-two-factor).
Lies docs/12-app-cloud/README.md und docs/12-app-cloud/ac-02-two-factor.md vollständig
und setze AC-02 genau danach um. Halte die Arbeitsweise aus dem README ein.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-02\REPORT.md im vorgegebenen Format.
```

## Ziel

Jede Person kann einen zweiten Faktor (TOTP-App wie Aegis, 1Password, Google Authenticator) einrichten. Eine Organisation kann ihn für alle Mitglieder verpflichtend machen. Der zweite Faktor gilt bei der Browser-Anmeldung, der Desktop-Anmeldung (AC-01) und in Launcher und Apps.

## Ist-Stand

- Keine MFA (Entscheidung „TOTP nach P3“, `docs/07-roadmap/phase-01-backend.md`, Threat Model „MFA later“).
- Nach AC-01: `POST /auth/device/login` kennt den Fehler `SECOND_FACTOR_REQUIRED` als Erweiterungsstelle.
- Passwort-Reset durch Admin-Link und Offline-CLI (Entscheidung P1).

## Entscheidungen (fest)

1. **Verfahren:** TOTP nach RFC 6238, HMAC-SHA-1, 6 Stellen, 30 s, Toleranz ±1 Zeitschritt. Ein Code wird je Konto höchstens einmal angenommen (letzten akzeptierten Zeitschritt speichern, gegen Wiederholung).
2. **Geheimnis:** 160 Bit Zufall. In der Datenbank nur verschlüsselt (AES-256-GCM oder ChaCha20-Poly1305 aus einer bereits vorhandenen oder sehr verbreiteten RustCrypto-Crate, Begründung im Bericht) mit einem Serverschlüssel aus `CRAFTCLOUD_SECRET_KEY_FILE` (32 Byte, Base64). Ohne Schlüsseldatei startet der Server, aber 2FA-Einrichtung antwortet 503 `TWO_FACTOR_UNAVAILABLE` und `/admin/health` meldet es. Schlüssel nie im Log; Rotation dokumentieren (zweiter, alter Schlüssel zum Entschlüsseln, `CRAFTCLOUD_SECRET_KEY_OLD_FILE`).
3. **Wiederherstellungscodes:** 10 Codes à 10 Zeichen aus einem eindeutigen Alphabet (ohne 0/O/1/l), nur als Argon2id- oder SHA-256-Hash mit Salt gespeichert, einmal verwendbar, bei Einrichtung genau einmal angezeigt, neu erzeugbar (alte ungültig).
4. **Einrichtung:** `POST /auth/2fa/setup` (angemeldet, Passwort erneut abfragen) liefert `otpauth://`-URI und Geheimnis in Base32 für manuelle Eingabe. QR-Code wird **im Browser** aus der URI erzeugt (kleine Bibliothek ohne Netzwerk oder eigener Encoder; keine externe URL, kein Server-Bild). `POST /auth/2fa/confirm` mit erstem Code aktiviert und liefert die Wiederherstellungscodes. `POST /auth/2fa/disable` mit Passwort und Code. `POST /auth/2fa/recovery-codes` mit Code erzeugt neue.
5. **Anmeldung Browser:** `POST /auth/login` antwortet bei aktivem 2FA mit 401 `SECOND_FACTOR_REQUIRED` und `challenge_id` (opak, 5 min gültig, an Konto und Client-Adresse gebunden, höchstens 5 Versuche). `POST /auth/login/second-factor` mit `{challenge_id, code}` oder `{challenge_id, recovery_code}` legt die Sitzung an. Ratenbegrenzung pro Konto und Adresse wie Login; falsche Codes zählen in die bestehende Sperre.
6. **Anmeldung Desktop:** gleiches Muster: `POST /auth/device/login/second-factor`.
7. **Organisationsrichtlinie:** `organizations.require_two_factor boolean`. Owner und Admin dürfen sie setzen (neue Aktion in `policy.rs`, Audit). Ist sie aktiv und hat ein Mitglied kein 2FA, darf es sich anmelden, sieht aber nur die Einrichtung (`403 TWO_FACTOR_SETUP_REQUIRED` auf allen Org-Routen dieser Organisation; Routen anderer Organisationen ohne Pflicht bleiben nutzbar). Kein Aussperren des letzten Owners: Wer die Richtlinie aktiviert, muss selbst 2FA haben.
8. **Zurücksetzen durch Admin:** Der vorhandene Admin-Reset-Link setzt auf Wunsch auch 2FA zurück (Option im Dialog, Audit `auth.two_factor_reset`). Offline-CLI `craftcloud-server admin user reset-2fa <email>` für den Operator.
9. **Sitzungen:** Aktivieren, Deaktivieren und Zurücksetzen von 2FA widerruft alle anderen Sitzungen und Geräte-Sitzungen des Kontos.
10. **Webpanel:** Einstellungen → „Zwei-Faktor-Anmeldung“: Status, Einrichten (QR + Geheimnis + Bestätigungscode), Wiederherstellungscodes anzeigen/herunterladen als Textdatei (lokal erzeugt), Deaktivieren. Login-Formular mit zweitem Schritt (Code oder Wiederherstellungscode). Org-Einstellung „2FA verpflichtend“. Design nach `docs/11-design/`, Texte EN/DE, axe EN/DE.
11. **Audit:** `auth.two_factor_enabled`, `auth.two_factor_disabled`, `auth.two_factor_failed` (aggregiert wie Login-Fehler), `auth.recovery_code_used`, `org.two_factor_policy_changed`.
12. **Backup:** Tabellen mit verschlüsselten Geheimnissen werden gesichert; der Runbook-Abschnitt sagt deutlich, dass ein Restore denselben `CRAFTCLOUD_SECRET_KEY_FILE` braucht.

## Erlaubte Dateien

`crates/craftcloud-auth/src/**` (neues Modul `totp.rs`, `secretbox.rs`), `crates/craftcloud-api/src/**` (Auth, Orgs, Policy-Anbindung), `crates/craftcloud-core/src/policy.rs`, `crates/craftcloud-db/**` (Migration `0017_two_factor.sql`), `crates/craftcloud-backup/src/tables.rs`, `apps/craftcloud-server/src/**` (Konfiguration, CLI), `apps/craftcloud-admin/src/**`, `contracts/openapi.yaml` + Snapshot, Doku wie in AC-01 plus `docs/06-operations/backup-and-restore.md` (falls vorhanden, sonst der Backup-Abschnitt in `deployment.md`), neues ADR `docs/09-decisions/adr-0019-two-factor.md`, `docs/05-security/permission-matrix.md`.

## Tests (Pflicht)

- Unit: RFC-6238-Testvektoren, Zeitfenster ±1, Wiederholungsschutz, Base32, Verschlüsselung rund und mit falschem Schlüssel, Schlüsselrotation.
- `cargo xtask db-test`: Einrichtung/Bestätigung/Deaktivierung, Login Browser und Desktop mit Code und mit Wiederherstellungscode, verbrauchter Code abgelehnt, Challenge-Ablauf und -Versuche, Ratenbegrenzung, Org-Pflicht sperrt nur diese Org, Owner kann Pflicht nicht ohne eigenes 2FA aktivieren, Admin-Reset, Widerruf der Sitzungen, Log-Test (kein Geheimnis, kein Code).
- Web: Komponenten- und Tastaturtests für Einrichtung und zweiten Login-Schritt, axe EN/DE.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Entscheidungen 1–12 umgesetzt oder begründet abweichend.
3. Kein TOTP-Geheimnis, Code oder Wiederherstellungscode im Klartext in DB, Log oder Audit (Test + Bericht).
4. QR-Code ohne Netzwerkzugriff und ohne CSP-Lockerung (`style-src 'self'`, kein Inline-Skript).
5. ADR-0019 angelegt.

## Nicht Teil von AC-02

WebAuthn/Passkeys, SMS oder E-Mail-Codes, „Gerät merken“ (2FA nur bei neuer Anmeldung – Geräte-Sitzungen aus AC-01 sind bereits langlebig), 2FA in Launcher-UI (AC-07 nutzt die Route).
