# AC-09b — E-Mail über Microsoft Graph und SMTP mit OAuth

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac09b`, Branch `feat/ac-09b-graph-mail` (Basis: `main` nach Merge von AC-09; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-09b\REPORT.md`. **Migration:** keine, außer der Bericht begründet eine. **Review:** unabhängig, sicherheitsrelevant (App-Berechtigungen beim Identitätsanbieter, Geheimnisse).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac09b (Branch feat/ac-09b-graph-mail).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-09-email-delivery.md,
docs/12-app-cloud/ac-09b-microsoft-graph-mail.md und das ADR von AC-09 vollständig
und setze AC-09b genau danach um. Halte die Arbeitsweise aus dem README ein.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-09b\REPORT.md im vorgegebenen Format.
```

## Ziel

Firmen mit Microsoft 365 oder Google Workspace können CraftCloud-Mails ohne Passwort-Anmeldung bei SMTP versenden. Hintergrund: Microsoft schaltet die Anmeldung mit Benutzername und Passwort bei SMTP in Exchange Online ab, und viele Firmennetze erlauben ausgehend nur HTTPS. AC-09b ergänzt dafür zwei Transporte hinter der Schnittstelle `MailTransport` aus AC-09; Warteschlange, Vorlagen, Verifikation und Ratenbegrenzung bleiben unverändert.

## Ist-Stand (bei Start prüfen)

- AC-09 gemerged: `MailTransport`, SMTP-Transport, Warteschlange `outbound_emails`, Testmail im Operator-Bereich, `/info` `feature_flags.email_delivery`.
- Serverschlüssel aus AC-02 für verschlüsselte Geheimnisse vorhanden.

## Entscheidungen (fest)

1. **Transport `graph` (Microsoft Graph):** `POST https://graph.microsoft.com/v1.0/users/{absender}/sendMail` mit App-Berechtigung `Mail.Send` (Client-Credentials-Flow, Token von `https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token`, Scope `https://graph.microsoft.com/.default`). Anmeldung bevorzugt mit Zertifikat (privater Schlüssel als Datei, signierte Client-Assertion), alternativ mit Client-Geheimnis als Datei. Konfiguration: `CRAFTCLOUD_MAIL_TRANSPORT=graph`, `CRAFTCLOUD_GRAPH_TENANT_ID`, `CRAFTCLOUD_GRAPH_CLIENT_ID`, `CRAFTCLOUD_GRAPH_CERT_FILE` + `CRAFTCLOUD_GRAPH_KEY_FILE` oder `CRAFTCLOUD_GRAPH_CLIENT_SECRET_FILE`, `CRAFTCLOUD_GRAPH_SENDER` (Absender-Postfach), optional `CRAFTCLOUD_GRAPH_SAVE_TO_SENT=false` (Standard). Nationale Clouds sind nicht Teil von AC-09b; die Endpunkte sind trotzdem als Konstanten an einer Stelle gebündelt.
2. **Transport `smtp` mit OAuth (XOAUTH2):** zusätzlicher Anmeldemodus des bestehenden SMTP-Transports, `CRAFTCLOUD_SMTP_AUTH=xoauth2`, mit Token-Anbieter `microsoft` (Client-Credentials, Scope `https://outlook.office365.com/.default`, Postfach-Berechtigung `SMTP.SendAsApp`) oder `google` (Dienstkonto mit domainweiter Delegierung, Scope `https://mail.google.com/`, signiertes JWT aus der Schlüsseldatei, Absender als `sub`).
3. **Tokens:** nur im Speicher, bis kurz vor Ablauf wiederverwendet (Puffer 5 Minuten), bei `401` einmal neu geholt. Nie in Logs, nie in der Datenbank, nie in Fehlermeldungen an Clients.
4. **Fehlerklassen:** `429` und `5xx` von Graph oder dem Tokenendpunkt sind vorübergehend (Warteschlange versucht erneut, `Retry-After` wird beachtet); `400`, `403` und ungültige Anmeldedaten sind endgültig für diese Mail und erscheinen im Operator-Bereich als Konfigurationsfehler mit dem Fehlercode des Anbieters, ohne Token oder Geheimnis.
5. **HTTP-Client:** dieselbe TLS-Basis wie der übrige Server (rustls, Plattform- oder webpki-Wurzeln); keine zweite TLS-Bibliothek. Neue Crates mit Version, Lizenz und Grund im Bericht; für JWT-Signaturen eine verbreitete Crate, keine eigene Kryptografie.
6. **Operator-Bereich:** zeigt den aktiven Transport, Absender, Ablaufzeit des Zertifikats (falls Zertifikat), letzte erfolgreiche Zustellung und den letzten Fehlercode; Testmail wie in AC-09.
7. **Betriebsdoku:** Schritt-für-Schritt in `docs/06-operations/install.md` für (a) Entra-App-Registrierung mit `Mail.Send`, Einschränkung auf das Absender-Postfach über RBAC for Applications in Exchange Online bzw. eine Application Access Policy, Zertifikat hochladen; (b) SMTP mit OAuth für Microsoft (`SMTP.SendAsApp`, Service Principal in Exchange registrieren); (c) Google-Dienstkonto mit domainweiter Delegierung. Hinweis, dass ohne Einschränkung `Mail.Send` aus jedem Postfach des Tenants senden darf.

## Erlaubte Dateien

`crates/craftcloud-mail/**`, `apps/craftcloud-server/src/**` (Konfiguration), `crates/craftcloud-api/src/**` (Operator-Status), `apps/craftcloud-admin/src/**` (System-Bereich), `deploy/compose/**`, `deploy/install/**`, `docs/06-operations/**`, `docs/05-security/threat-model.md`, das ADR von AC-09 (Abschnitt „Ergänzung AC-09b“), `docs/gaps.md`.

## Tests (Pflicht)

- Unit: Konfigurationsprüfung (fehlende oder widersprüchliche Variablen ergeben eine klare Startmeldung), Token-Wiederverwendung und Erneuerung, Fehlerklassen, Client-Assertion-Format.
- Integration gegen nachgebaute Dienste im Test (lokaler HTTPS-Mock für Tokenendpunkt und Graph `sendMail`; SMTP-Testserver mit XOAUTH2): Erfolg, abgelaufenes Token, `429` mit `Retry-After`, `403`, falscher Absender.
- Secret-Scan der Testausgaben: keine Tokens, Geheimnisse oder Schlüssel.
- Ein Test gegen einen echten Microsoft- oder Google-Tenant ist **nicht** Pflicht; der Bericht nennt ihn als offene Prüfung für den Projektverantwortlichen.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Umschalten zwischen `smtp`, `smtp` mit `xoauth2` und `graph` nur über Konfiguration; Warteschlange und Vorlagen unverändert.
3. Keine Geheimnisse in Logs, Antworten oder Datenbank.
4. Betriebsdoku vollständig genug, dass ein Admin die Entra-App ohne weitere Quelle einrichten kann.
5. Entscheidungen 1–7 umgesetzt oder begründet abweichend.

## Nicht Teil von AC-09b

Gmail-API, Versanddienst-spezifische HTTP-APIs (SendGrid, Mailgun usw. laufen über SMTP), Empfang von Mails, delegierte Anmeldung einer Person statt App-Berechtigung, nationale Microsoft-Clouds.
