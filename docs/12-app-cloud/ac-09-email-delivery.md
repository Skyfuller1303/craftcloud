# AC-09 — E-Mail-Versand und Adressverifikation

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac09`, Branch `feat/ac-09-email` (Basis: aktueller `main`; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-09\REPORT.md`. **Migration:** die nächste freie Nummer, die der Koordinator beim Start nennt (0018 ist für AC-06 reserviert). **Review:** unabhängig, sicherheitsrelevant (Kontoerstellung, Links per E-Mail).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac09 (Branch feat/ac-09-email).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-09-email-delivery.md und
docs/09-decisions/adr-0017-email-verification-deferral.md vollständig und setze AC-09 genau danach um.
Halte die Arbeitsweise aus dem README ein. Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-09\REPORT.md im vorgegebenen Format.
```

## Ziel

CraftCloud verschickt E-Mails über einen vom Betreiber konfigurierten SMTP-Server: Einladungen, Passwort-Reset-Links, Hinweise auf sicherheitsrelevante Kontoänderungen und die Bestätigung einer E-Mail-Adresse. Damit wird die in ADR-0017 verschobene Adressverifikation nachgeholt und das dort beschriebene „Account-Squatting“ geschlossen. Ohne SMTP-Konfiguration verhält sich die Instanz genau wie heute (Links werden im Webpanel angezeigt und kopiert).

## Ist-Stand (bei Start prüfen)

- Einladungen und Reset-Links entstehen ohne Versand; der Besitz des Links gilt als Nachweis (ADR-0013 Punkt 5). Lebensdauer: Einladung 72 h, Reset 1 h.
- `users.email_verified_at` existiert und bleibt heute leer; Operator-Übersicht `GET /api/v1/admin/users?unverified=true`.
- E-Mail-Änderung ist bisher nur durch den Operator möglich (AC-08b); Anzeigename ändert jede Person selbst.
- Konfiguration nach ADR-0011: Geheimnisse nur als Datei (`*_FILE`), nie in Logs.

## Entscheidungen (fest)

1. **Transport:** Versand über eine austauschbare Schnittstelle (Trait `MailTransport` mit `send(message) -> Result<Delivery, TransportError>`, Fehler unterschieden in vorübergehend und endgültig), damit AC-09b Microsoft Graph und SMTP mit OAuth ohne Umbau ergänzen kann. AC-09 liefert nur den SMTP-Transport: STARTTLS oder implizites TLS, Zertifikatsprüfung immer an (keine Option zum Abschalten außer für Loopback in Tests), Anmeldung mit Benutzername und Passwort **oder** ohne Anmeldung für Relays mit IP- oder Zertifikatsfreigabe (Microsoft-365-Connector, Google-SMTP-Relay, eigener Postfix). Konfiguration: `CRAFTCLOUD_MAIL_TRANSPORT=smtp`, `CRAFTCLOUD_SMTP_HOST`, `CRAFTCLOUD_SMTP_PORT`, `CRAFTCLOUD_SMTP_TLS` (`starttls` | `tls`), optional `CRAFTCLOUD_SMTP_USERNAME` und `CRAFTCLOUD_SMTP_PASSWORD_FILE` (beide oder keiner), `CRAFTCLOUD_SMTP_FROM` (Adresse und Anzeigename). Fehlt `CRAFTCLOUD_MAIL_TRANSPORT`, ist der Versand aus; `/info` meldet `feature_flags.email_delivery`. `docs/06-operations/install.md` beschreibt Einrichtungsbeispiele für eigenen Mailserver, einen Versanddienst über SMTP, den Microsoft-365-Connector und das Google-SMTP-Relay.
2. **Bibliothek:** eine verbreitete Rust-SMTP-Crate (Empfehlung `lettre` mit rustls, ohne native-tls); Version, Lizenz und Grund im Bericht.
3. **Ausgangswarteschlange:** E-Mails werden in einer Tabelle `outbound_emails` (Empfänger, Vorlage, Sprache, Parameter ohne Tokens im Klartext, Status, Versuche, nächster Versuch) in derselben Transaktion angelegt wie die auslösende Aktion. Ein Hintergrund-Worker im Server versendet sie mit exponentiellem Backoff (höchstens 8 Versuche in 24 h), danach Status `failed` und Audit. Der Link-Token selbst wird nicht gespeichert, sondern beim Versand aus einem verschlüsselten Feld gelesen (Serverschlüssel aus AC-02, eigenes AAD `"email"`) und nach Versand oder Ablauf gelöscht.
4. **Vorlagen:** Text und HTML, Englisch und Deutsch, als Dateien im Repository (keine Vorlagen-Engine mit Code-Ausführung; einfache, getestete Platzhalterersetzung mit HTML-Escaping). Sprache aus der Einladung oder der Kontoeinstellung, sonst Englisch. Kein Tracking, keine externen Bilder.
5. **Adressverifikation:**
   - Einladung an eine Adresse ohne Konto: Das Konto wird erst angelegt, wenn die Person den per E-Mail gesendeten Link öffnet und ein Passwort setzt; `email_verified_at` wird dabei gesetzt.
   - Mit aktivem Versand zeigt das Webpanel den Einladungslink nicht mehr an (nur „gesendet an …“ und „erneut senden“). Ohne Versand bleibt das heutige Verhalten.
   - Bestehende unverifizierte Konten: Beim nächsten Login erscheint ein Hinweis mit „Bestätigungslink senden“. Der Operator kann die Bestätigung für alle unverifizierten Konten anstoßen.
   - Einladungen an ein bestehendes, unverifiziertes Konto einer anderen Organisation: erst nach dessen Verifikation annehmbar (Antwort an den Einladenden ohne Hinweis auf Kontoexistenz: „Einladung gesendet“).
6. **E-Mail-Änderung durch die Person selbst:** neue Route; Bestätigung an die neue Adresse (Link 24 h), Hinweis an die alte Adresse ohne Link. Erst nach Bestätigung wird die Adresse übernommen. Passwort erforderlich; bei aktivem 2FA zusätzlich ein Code.
7. **Sicherheitshinweise per E-Mail:** Passwort geändert, 2FA aktiviert oder deaktiviert, neue Geräte-Anmeldung, E-Mail geändert. Ohne Links, die eine Aktion auslösen; nur Hinweis und „Wenn du das nicht warst, wende dich an …“.
8. **Missbrauchsschutz:** Ratenbegrenzung je Empfänger (höchstens 5 Mails je Stunde je Vorlage) und je auslösender Person; „erneut senden“ höchstens einmal je Minute. Antworten verraten nie, ob ein Konto existiert.
9. **Operator:** Bereich „System“ zeigt Versandstatus (konfiguriert, letzte erfolgreiche Zustellung, Anzahl fehlgeschlagener Mails) und eine Testmail an die eigene Adresse. Keine Anzeige von Mailinhalten.
10. **Betrieb:** `deploy/compose` und `deploy/install` bekommen die SMTP-Variablen (Passwort als Secret-Datei wie die anderen Geheimnisse); `docs/06-operations/install.md` beschreibt die Einrichtung. Für lokale Tests ein SMTP-Testserver im Container (z. B. Mailpit), nur in Tests und Dev-Compose, nie im Installationspfad.

## Erlaubte Dateien

`crates/craftcloud-api/src/**`, `crates/craftcloud-db/**` (Migration), `crates/craftcloud-core/src/**`, neue Crate `crates/craftcloud-mail` (Versand, Vorlagen), `apps/craftcloud-server/src/**`, `apps/craftcloud-admin/src/**`, `contracts/openapi.yaml` und Snapshot, `deploy/compose/**`, `deploy/install/**`, `docs/03-features/cc-f02-identity-and-sessions.md`, `docs/03-features/cc-f03-users-and-invitations.md`, `docs/05-security/threat-model.md`, `docs/05-security/permission-matrix.md`, `docs/06-operations/**`, neues ADR `docs/09-decisions/adr-0025-email-delivery.md` (ersetzt ADR-0017), `docs/gaps.md`.

## Tests (Pflicht)

- Unit: Vorlagen-Escaping (HTML-Injection im Anzeigenamen), Platzhalter, Sprachwahl, Backoff.
- DB/HTTP: Warteschlange in derselben Transaktion (Rollback ohne Mail; Sicherheitshinweise blockieren die Aktion nie, ein Limit führt zu einem gezählten unterdrückten Hinweis), Verifikationsablauf für neue und bestehende Konten, E-Mail-Änderung mit Bestätigung, keine Kontoexistenz-Leaks, Ratenbegrenzung, Token nach Versand gelöscht.
- Integration gegen einen SMTP-Testcontainer: echter Versand, TLS-Pflicht, Fehlerfall mit Wiederholung.
- Web: Einladungs- und Profilbildschirme mit und ohne Versand, EN/DE, axe.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Squatting aus ADR-0017 ist mit aktivem Versand nicht mehr möglich (Test belegt).
3. Ohne SMTP-Konfiguration bleibt das bisherige Verhalten unverändert (Test belegt).
4. Keine Tokens, Passwörter oder Mailinhalte in Logs; Secret-Scan der Testausgaben sauber.
5. Entscheidungen 1–10 umgesetzt oder begründet abweichend; ADR-0025 angelegt.

## Nicht Teil von AC-09

Newsletter oder Massenmails, Benachrichtigungen über Dateiänderungen, eingehende Mails, DKIM-Signatur durch CraftCloud (macht der SMTP-Server).
