# CC-F13 — Gemeinsames Client SDK und Server-Discovery

> **Status:** AC-04 implementiert die drei SDK-/Widget-Crates, Discovery und eine lokale Demo; die Abnahmebelege und verbleibenden Einschränkungen stehen im AC-04-Bericht. App-Integration folgt in AC-05. **Zielphase:** P4 (Client SDK / first app). **Priorität:** nach API-Gate.
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

> **Neuplanung 2026-10-30:** Konkrete Umsetzung in [docs/12-app-cloud](../12-app-cloud/README.md) (AC-04, AC-05).

## Problem und Nutzen

Craft-Apps erhalten einen kleinen Rust-Client mit Discovery, authentifizierten Requests, Workspaces/Files, Upload-Resume, CAS-Speichern, Timeout, Cancel und verständlichen Fehlertypen. SDK darf App-Modelle nicht enthalten und keine globale Cloudpflicht erzeugen. Base URL ist vom Nutzer/Unternehmen konfiguriert und wird nach URL/Origin-/TLS-Policy validiert.

## Detaillierter Funktionsumfang und technische Umsetzung

`craftcloud-client` verwendet einen transportbasierten, I/O-freien Ablauf; `craftcloud-client-native` übernimmt HTTP/TLS, Schlüsselbund, lokale Verknüpfungen und Cache; `craftcloud-egui` liefert übersetzbare Widgets mit den Visuals der Gast-App. `GET /api/v1/info` publiziert Versionen, Funktionsschalter, Grenzen und Formatregister. Desktop-Geräte-Sitzungen verwenden den genehmigten AC-01-Login; die reservierte 2FA-Challenge wird erkannt, ihre serverseitige Fortsetzung hängt von AC-02 ab. Kein globaler API-Key wird gespeichert. Siehe [SDK-Vertrag](../04-integrations/sdk-contract.md) und [ADR-0021](../09-decisions/adr-0021-client-sdk.md).

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

TLS mit self-signed/private CA, Server wechselt Host, 401 während Upload, Upgrade inkompatibler API, Server untrusted internal IP/SSRF durch Auto-Discovery, Offline queue duplication, Timeouts, HTTP redirects mit Token, canonical URL confusion.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

SDK hat ohne Cloud keinerlei Einfluss auf lokale App. Dokumentierter sample client kann Datei roundtrip mit eingeengter Berechtigung. Matrix deckt Windows/macOS/Linux/WASM (soweit unterstützt) und Fehlerfälle ab.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**
