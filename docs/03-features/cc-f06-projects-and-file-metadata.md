# CC-F06 — Projekte, Verzeichnisse, Dateimetadaten

> **Status:** geplantes Feature-Briefing, nicht implementiert. **Zielphase:** P2 (Files & Versioning). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Projekte bilden die neutrale Ablagestruktur. Dateien haben stabile IDs, Benutzername, zugehörige App (z.B. `figcraft`), Formatkennung, Byteanzahl, Hash und Head-Version. Erstes MVP nutzt eine flache Projektdateiliste; Ordnerhierarchie erst, wenn Rechte-/Move-Semantik geklärt ist. Dateiname ist untrusted display metadata und darf niemals zum direkten Speicherpfad werden.

## Detaillierter Funktionsumfang und technische Umsetzung

`POST /workspaces/{id}/projects`, `GET /.../projects`, `GET/PATCH /projects/{id}`, `POST /projects/{id}/files` (Metadaten abhängig vom Upload-Protokoll), `GET /projects/{id}/files`, `GET/PATCH /files/{id}`. Paginierung mit Scope-gebundenem Cursor. Soft-delete mit definiertem Restore-Fenster.

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

Unicode normalization, sehr langer Dateiname, mehrfach gleiche Name, `../`, Nullbyte, reserved Windows names, unterschiedliche App-Versionen, Project rename/move und gleichzeitige Datei-Commits. Kein Formatversprechen durch Dateiendung allein.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Projekt A/B-ACLs werden eingehalten; B kann keine Dateimetadaten aus A erraten; Dateiname verändert nie den OS-Pfad; sortierte und stabile Pagination funktioniert auch bei neuen Dateien.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
