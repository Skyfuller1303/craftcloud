# CC-F03 — Benutzerverwaltung und Einladungen

> **Status:** Einladungen lokal umgesetzt (PR-007, Review offen): Link-Einladung (72 h, einmalig, widerrufbar, nur SHA-256 gespeichert, kein E-Mail-Versand), Annahme mit neuem oder bestehendem Konto, Neubewertung der Rechte des Einladenden bei der Annahme, Limits und Ratenbegrenzung. E-Mail-Verifikation und Versand (SMTP) sind offen (Lücke). Ursprünglich: geplantes Feature-Briefing. **Zielphase:** P1 (Backend Foundations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Organisationsadministrator lädt Nutzer per E-Mail zu einer Organisation oder konkret zu einem Workspace ein. Externe E-Mail-Sendeinfrastruktur ist optional; MVP kann einen einmalig kopierbaren Invite-Link bereitstellen, wenn Mailserver nicht konfiguriert ist. Mitgliedsstatus invited/active/disabled und sichtbare Einladungsübersicht im Adminpanel.

## Detaillierter Funktionsumfang und technische Umsetzung

`POST /organizations/{org}/invitations`, `GET .../invitations`, `POST /invitations/accept`, `DELETE /.../invitations/{id}`. Invitation = target_email normalized, org scope, target role, hash(token), expiry, accepted/revoked, created_by. Der Empfänger muss die gebundene E-Mail-Adresse kontrollieren und eine gültige authentifizierte Sitzung besitzen, bevor ein Beitritt erfolgt. E-Mail-Verification-Methode gesondert spezifizieren.

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

Invites zu schon bestehendem Account, mehrfacher Annahme, fremde E-Mail, Invite nach Org-Deletion, Rolleneskalation beim Accept, unlimitierte Invites, Token in Referer/Logs, 2 Admins laden gleichzeitig mit unterschiedlicher Rolle ein.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Ein gültiger Invite kann genau einmal angenommen werden; fremde Konten können ihn nicht missbrauchen; Einladung zeitlich begrenzt, widerrufbar und tenant-scoped; Admin sieht Status und Audit-Eintrag.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
