# CC-F19 — Unternehmens-SSO und optional SCIM

> **Status:** AC-10 implementiert die lokale OIDC-/SCIM-Funktion; unabhängige Abnahme und tatsächliche Prüfergebnisse stehen im Aufgabenbericht. **Zielphase:** P6 (Enterprise / extensions). **Priorität:** später / nur nach Gate.
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Unternehmen integrieren vorhandenen IdP über OIDC Authorization Code mit PKCE. Lokaler Admin-Recovery-Zugang bleibt betreiberkontrolliert. Benutzerzuordnung über stabile Issuer+Subject IDs, niemals nur E-Mail, da E-Mail wechselbar. Rollen und Gruppen-Provisioning separat konzeptionieren und nicht bei Erstlogin automatisch Owner-Rechte vergeben.

## Detaillierter Funktionsumfang und technische Umsetzung

`/auth/oidc/start`, `/auth/oidc/callback` mit Discovery-URL, verifiziertem issuer/audience/signature/nonce/state/PKCE; JWK-Rotation, redirect allowlist, tenant-bound configuration. SCIM 2.0 erst später als getrennte, gesicherte Provisioning-Schnittstelle mit klarer precedence gegenüber manuellen Rollen.

## User Journey und gewünschtes Verhalten

1. Client öffnet die zugehörige Oberfläche oder ruft die versionierte API auf; authentifiziert sich nur, wenn Cloudzugriff gewünscht ist.
2. API validiert Pflichtfelder, Request-Größe, Resource Scope und Actor-Berechtigung; unautorisierte Zugriffe müssen **vor** kostenintensiven Storage-/DB-Aktionen stoppen.
3. Die Domänenschicht führt die Operation mit eindeutiger Ownership und klar dokumentiertem Statusübergang aus.
4. Client erhält eine stabile Response mit maschinenlesbarem Fehlercode und Request-ID; Retry ist nur bei nachweislich idempotenter Aktion automatisch zulässig.
5. Negative Fälle werden als Fehler **mit Recovery-Pfad** behandelt; lokale Craft-Dateien bleiben erhalten.

## Daten- und Rechtevertrag

- **Objekt-Scope:** jedes betroffene Objekt erhält `org_id`/Workspace-Kontext; für User- oder Serverobjekte wird Instanzscope explizit geprüft.
- **Zulässige Rollen:** durch serverseitiges Policy-Modul und `permission-matrix.md` festzulegen/umzusetzen; UI-Sichtbarkeit ist kein Schutz.
- **Audit:** sensitive Schreib-/Berechtigungsaktionen mit Actor, Ziel, Resultat und Request-ID; niemals Passwort, Invite-Token oder Dateibytes loggen.
- **Transaktionen:** Metadatenänderungen konsistent; externe Storage-Operationen benötigen ein abgestimmtes Recovery-/Compensation-Design.
- **Versionierung:** API-Kontrakt ändern nur mit kompatibler Migration und Client-Upgrade-Matrix.

## Risiken, Grenzfälle und Fehlerbehandlung

IdP down, key rotation, User E-Mail gewechselt, issuer spoofing, login from wrong organization, stale groups, SAML vs OIDC expectation, recovery admin bypass, logout and revoked identities.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Test-IdP kann Accounts provisionieren/deprovisionieren; fremder IdP ist abgewiesen, Konten werden nicht nach E-Mail kollidierend zusammengeführt; lokale Datei-Funktion bleibt cloud-unabhängig.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**


## Lokaler Implementierungsstand AC-10

[ADR-0026](../09-decisions/adr-0026-sso-and-scim.md) konkretisiert den Vertrag. Die [Betriebsanleitung](../06-operations/sso-and-scim.md) beschreibt Konfiguration, Kontobindung, MFA, Desktop und den umgesetzten SCIM-Umfang einschließlich seiner Grenzen. Migration 0021 ergänzt organisationseigene Provider, stabile Identitätsbindungen und Provisionierungszustand. Die Bindung bei E-Mail-Wechsel wird durch den echten Container-IdP in `crates/craftcloud-api/src/oidc_tests.rs` geprüft; HTTP-Provisionierung durch `crates/craftcloud-api/tests/scim.rs`. Ein Testname allein ist kein Ausführungsnachweis: maßgeblich sind die Logs und der Abschlussbericht von AC-10.
