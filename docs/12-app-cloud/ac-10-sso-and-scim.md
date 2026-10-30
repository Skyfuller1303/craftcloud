# AC-10 — Unternehmens-Anmeldung (OIDC) und Provisionierung (SCIM)

> **Repository:** CraftCloud. **Worktree:** `D:\ArtCraft\CraftCloud\wt-ac10`, Branch `feat/ac-10-sso-scim` (Basis: aktueller `main` nach AC-09; legt der Koordinator an). **Bericht:** `D:\ArtCraft\output\craftcloud-codex\AC-10\REPORT.md`. **Migration:** die nächste freie Nummer, die der Koordinator beim Start nennt. **Review:** unabhängig, sicherheitsrelevant (Identität, Kontozuordnung, Provisionierung).

## Prompt für Codex

```
Arbeite im Worktree D:\ArtCraft\CraftCloud\wt-ac10 (Branch feat/ac-10-sso-scim).
Lies docs/12-app-cloud/README.md, docs/12-app-cloud/ac-10-sso-and-scim.md und
docs/03-features/cc-f19-oidc-sso-and-provisioning.md vollständig und setze AC-10 genau danach um.
Halte die Arbeitsweise aus dem README ein. Migration: <Nummer vom Koordinator>.
Cargo nur über den Speicher-Launcher mit dem vom Koordinator genannten --gib-Wert.
`cargo xtask ci` und `cargo xtask db-test` müssen wirklich laufen.
Schreibe am Ende D:\ArtCraft\output\craftcloud-codex\AC-10\REPORT.md im vorgegebenen Format.
```

## Ziel

Eine Organisation verbindet ihren eigenen Identitätsanbieter (Microsoft Entra ID, Google Workspace, Okta, Keycloak, Authentik …). Personen melden sich im Webpanel, im Launcher und in den Apps mit dem Firmenkonto an. Optional legt der Identitätsanbieter Personen per SCIM 2.0 automatisch an, ändert und deaktiviert sie. Lokale Anmeldung mit E-Mail und Passwort bleibt für den Operator und als Notfallzugang erhalten.

## Ist-Stand (bei Start prüfen)

- Anmeldung nur lokal (Argon2id-Passwort, optional TOTP), Browser-Sitzungen und Geräte-Sitzungen (AC-01) mit rotierendem Erneuerungstoken.
- CC-F19 beschreibt die Anforderungen (OIDC Authorization Code mit PKCE, Bindung an Issuer + Subject, nie nur an die E-Mail).
- Konten sind instanzweit; eine Person kann mehreren Organisationen angehören.

## Entscheidungen (fest)

1. **Protokoll:** nur OIDC Authorization Code mit PKCE (S256), `state` und `nonce` Pflicht, ID-Token-Signatur über JWKS mit Schlüsselrotation, Prüfung von `iss`, `aud`, `exp`, `iat`, `nonce`. Kein Implicit Flow, kein SAML (später eigenes Briefing, falls nötig). Bibliothek: eine verbreitete Rust-OIDC-Crate (Empfehlung `openidconnect`); Version, Lizenz und Grund im Bericht.
2. **Konfiguration je Organisation:** Owner richtet einen Anbieter ein (Discovery-URL, Client-ID, Client-Geheimnis verschlüsselt mit dem Serverschlüssel, erlaubte E-Mail-Domains). Der Operator kann OIDC instanzweit erlauben oder verbieten (Standard: erlaubt). Die Redirect-URI ist fest `<public_url>/api/v1/auth/oidc/callback`, keine frei wählbaren Ziele.
3. **Kontozuordnung:** Tabelle `external_identities (org_id, issuer, subject, user_id)`, eindeutig je `(issuer, subject)`. Erstanmeldung:
   - Gibt es bereits eine Zuordnung, wird dieses Konto verwendet, egal welche E-Mail der Anbieter meldet.
   - Gibt es keine, aber ein Konto mit derselben, verifizierten E-Mail (AC-09), wird die Zuordnung erst nach einer Anmeldung mit dem lokalen Passwort dieses Kontos angelegt („Konten verknüpfen“). Keine stille Zusammenführung über die E-Mail.
   - Gibt es keins, wird ein Konto angelegt, wenn die Domain erlaubt ist und die Organisation „Konten bei Erstanmeldung anlegen“ aktiviert hat; Rolle `member`, nie Owner oder Admin.
4. **Durchsetzung:** Owner kann „Anmeldung nur über SSO“ aktivieren. Dann lehnt die Organisation lokale Passwort-Sitzungen ihrer Mitglieder für ihre Ressourcen ab (wie die 2FA-Pflicht aus AC-02, mit eigenem Fehlercode `SSO_REQUIRED`). Owner behalten einen lokalen Notfallzugang; der Operator ist nie betroffen.
5. **Desktop:** Launcher und Apps nutzen OIDC über den Systembrowser mit Loopback-Redirect (RFC 8252) und PKCE und erhalten danach eine normale Geräte-Sitzung (AC-01). Das SDK (`craftcloud-client`) bekommt dafür einen Ablauf ohne eingebetteten Browser. Kein Passwort des Anbieters in einer App.
6. **2FA:** Bei SSO-Anmeldung entscheidet der Anbieter über MFA; die CraftCloud-2FA-Pflicht der Organisation gilt dann als erfüllt, wenn der Owner das so einstellt (Standard: erfüllt, wenn das ID-Token `amr` mit `mfa` enthält, sonst zusätzlich TOTP).
7. **Abmeldung und Entzug:** Lokale Sitzungen enden wie bisher. Deaktiviert SCIM eine Person, werden alle ihre Browser- und Geräte-Sitzungen in der Organisation sofort ungültig und ihre Sperren freigegeben. Front-Channel- oder Back-Channel-Logout des Anbieters ist nicht Teil von AC-10.
8. **SCIM 2.0:** Endpunkt `/scim/v2/organizations/{org_id}/Users` und `/Groups` (RFC 7643/7644: Create, Get, List mit `filter=userName eq`, Replace, Patch, Delete), Authentifizierung über ein eigenes, nur einmal angezeigtes SCIM-Token je Organisation (gehasht gespeichert, rotierbar). Gruppen werden auf Workspace-Mitgliedschaften abgebildet, die der Owner pro Gruppe festlegt (Gruppe → Workspace und Rolle `viewer` oder `editor`; nie Org-Admin). Manuell gesetzte Rollen bleiben, solange SCIM sie nicht ausdrücklich verwaltet; die Vorrangregel steht im ADR.
9. **Webpanel:** Einstellungen der Organisation bekommen „Anmeldung“ (Anbieter einrichten und testen, erlaubte Domains, SSO-Pflicht, Erstanmeldung) und „Provisionierung“ (SCIM-Token erzeugen und widerrufen, Gruppenzuordnung, letzte Ereignisse). Die Anmeldeseite zeigt „Mit Firmenkonto anmelden“ nach Eingabe der E-Mail, wenn deren Domain einer Organisation mit SSO gehört.
10. **Audit:** Anmeldung über SSO, Kontoverknüpfung, Änderungen an der Konfiguration, jede SCIM-Operation (ohne Token, ohne vollständige Nutzlast).

## Erlaubte Dateien

`crates/craftcloud-api/src/**` (Module `oidc.rs`, `scim.rs`), `crates/craftcloud-db/**` (Migration), `crates/craftcloud-core/src/**`, `crates/craftcloud-auth/**`, `crates/craftcloud-client/**` und `crates/craftcloud-client-native/**` (Desktop-Ablauf), `apps/craftcloud-server/src/**`, `apps/craftcloud-admin/src/**`, `contracts/openapi.yaml` und Snapshot, `docs/03-features/cc-f19-oidc-sso-and-provisioning.md`, `docs/04-integrations/sdk-contract.md`, `docs/05-security/**`, `docs/06-operations/**`, neues ADR `docs/09-decisions/adr-0026-sso-and-scim.md`, `docs/gaps.md`.

## Tests (Pflicht)

- Integration gegen einen echten OIDC-Testanbieter im Container (Empfehlung Keycloak oder ein kleiner Mock-IdP mit JWKS): Erfolg, falscher Issuer, falsche Audience, abgelaufenes Token, falsches `nonce`, `state`-Wiederholung, Schlüsselrotation, E-Mail-Wechsel beim Anbieter, Domain nicht erlaubt.
- Kontozuordnung: keine Übernahme eines bestehenden Kontos nur über die E-Mail.
- SCIM: Konformität der Grundoperationen, Deaktivierung beendet Sitzungen und Sperren, Token falsch oder widerrufen, fremde Organisation.
- Desktop-Ablauf im SDK mit Mock-Browser und Loopback-Redirect.
- Web: neue Einstellungen EN/DE mit axe.

## Abnahme

1. `cargo xtask ci` und `cargo xtask db-test` grün (selbst ausgeführt).
2. Exit-Gate aus Phase 6: „OIDC subject binding is safe against email reassignment“ durch Test belegt.
3. SSO-Pflicht und Notfallzugang funktionieren; der Operator bleibt immer lokal anmeldbar.
4. Keine Geheimnisse (Client-Geheimnis, SCIM-Token, ID-Token) in Logs oder Antworten außer der einmaligen Token-Anzeige.
5. Entscheidungen 1–10 umgesetzt oder begründet abweichend; ADR-0026 angelegt.

## Nicht Teil von AC-10

SAML, LDAP/Active-Directory-Direktanbindung, Logout-Weiterleitung an den Anbieter, Verschachtelte Gruppen, instanzweite Anbieter für alle Organisationen.
