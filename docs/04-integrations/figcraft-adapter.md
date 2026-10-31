# FigCraft-Integration: Abhängigkeiten und Machbarkeitsgate

## AC-06: lokaler Begleitdienst

[ADR-0022](../09-decisions/adr-0022-figcraft-live-hosting.md) konkretisiert die bisher zurückgestellte Integration: Der unveränderte FigCraft-Commit `ddda495a6ff40c326a117fbc393a8cfaa319f529` wird mit Protokoll 2 und Wire-Version 1 betrieben. CraftCloud verwaltet Datei und persönliche Edit-/View-Links per Serverschlüssel. Es gibt weiterhin keine CraftCloud-Konten im FigCraft-Protokoll und keinen Ersatz der Live-Engine.

Die vier Routen `GET/POST/DELETE /api/v1/files/{id}/live` und `POST /api/v1/files/{id}/live/snapshot` sind im OpenAPI-Vertrag beschrieben. Veröffentlichung benötigt Leserecht, ein Edit-Link und eine manuelle Sicherung benötigen Bearbeitungsrecht, Beenden benötigt Workspace-Admin-Recht. Die Oberfläche zeigt die Funktion nur bei `info.feature_flags.figcraft_live`; Autorisierung erfolgt trotzdem für jede Anfrage auf dem Server. Erneuern eines persönlichen Links widerruft den bisherigen. Ein Link bleibt ein übertragbares Bearer-Geheimnis, kein Nachweis der Identität seiner tatsächlich verbundenen Person.

Der Spike bestätigt Veröffentlichung eines nativen Dokuments, Editor-/Viewer-Trennung, Widerruf, Online-Export, Neustartpersistenz und `/figcraft/` als Pfad. Der Adapter erhält nicht synchronisierte Archivteile aus der ursprünglichen Cloud-Version. Die Grenze von 64 MiB und das Ablehnen reparaturbedürftiger Dokumente verhindern einen stillen verlustbehafteten Import. Laufende Änderungen werden als unveränderliche Versionen mit `actor_kind = figcraft_live` gesichert; gewöhnliche Uploads und Restore auf dieselbe Datei liefern währenddessen `409 FILE_IS_LIVE`.

Die transaktionale Outbox trägt Rechteentzug über Prozessabstürze hinweg. Wiederholungen und der fünfminütige Rechteabgleich beheben vorübergehende Ausfälle; während eines FigCraft-Ausfalls ist keine feste maximale Widerrufslatenz garantiert. Betrieb, Schlüssel und Restore sind in [Deployment](../06-operations/deployment.md) und im [Backup-Runbook](../06-operations/backup-upgrade-runbook.md) beschrieben. Native Capability-Prüfung sowie die Abstimmung mit FigCraft und CraftHub bleiben offen. Die folgenden Varianten und Fragen dokumentieren den früheren Planungsstand; für den lokal implementierten Umfang gilt ADR-0022.

## Verifizierter Stand

Die [FigCraft Server-Dokumentation](https://github.com/storytold/figcraft/blob/main/docs/server.md) beschreibt persistente Dokumente, Snapshots + Operationslog, WebSocket, Owner-Token/Server-Key, Bearbeiten/Lesen-Freigaben, Revocation, TLS-Reverse-Proxy, Team Libraries und Plugin Directory. **Es gibt aktuell keine Benutzeraccounts oder per-user Permissions**. Die [Roadmap](https://github.com/storytold/figcraft/blob/main/docs/roadmap.md) nennt Accounts/Teams ausdrücklich als ausstehende M9-Arbeit. `figcraft-server` speichert Dateien auf dem Server derzeit unverschlüsselt und ist nicht E2EE.

## Architekturentscheidung

Eine neue Cloud darf das FigCraft-Sessionprotokoll nicht heimlich ersetzen. Erstes CraftCloud-Projekt kann eine `.figcraft` als opakes Blob speichern; das ist **Dateiversionierung**, nicht Live-Editing. Anbindung von Sessions braucht separate RFC zu Session-/Identity-Bindings.

## Variantenvergleich

| Variante | Ansatz | Vorteil | Risiko |
|---|---|---|---|
| A | Benutzer öffnet manuell FigCraft Share Link aus Cloud | keine Protokolländerung | kein einheitliches Rechte-/Revocation-Modell |
| B | FigCraft-Server prüft CraftCloud Capability | saubere AuthZ-Integration | Anpassung des FigCraft-Protokolls, Maintainer-Review |
| C | Authenticated Gateway vor dem FigCraft-Server | keine native Serveränderung möglich | Gateway-Reconnect/WS/Session-Security komplex |
| D | FigCraft-Collab in CraftCloud nachimplementieren | gemeinsamer Dienst | riesiger Scope, hohe Duplikation, nicht empfohlen |

**Empfehlung:** B als langfristiger gemeinsamer Entwurf, A nur als klar getrennte manuelle Verlinkung. Keine automatische Browser-Session-Brücke ohne Token-Risikoanalyse.

## Protokollfragen

- Wer ist Authorität für User/Org/Workspace und Dateieigentümer?
- Wie wird ein CraftCloud-Projekt auf eine FigCraft-Datei abgebildet, ohne doppelte Kopien auseinanderlaufen zu lassen?
- Wie bekommt ein Viewer ausschließlich View-Rechte? In welcher Zeit greift Revocation bei bestehender WebSocket-Session?
- Wie werden offline Operationslogs und CraftCloud Versionierung konsistent verknüpft?
- Wer darf eine Team-Library aktivieren und anderen Projekten bereitstellen?
- Wie werden wiederaufgenommene Sessions nach Router-/Serverrestart autorisiert?

## Proof-of-concept Gate

1. Maintainer-Signoff zum Protokoll.
2. Prototyp nur mit synthetischen Dokumenten/Accounts.
3. Editor/Viewer/Revoke/Disconnect/Reconnect funktionieren reproduzierbar.
4. Alte FigCraft-Links bleiben kompatibel oder sind explizit versioniert.
5. Kein Zugriff des Cloud-Servers auf privates FigCraft-Dateiverzeichnis ohne dokumentierte, sichere Schnittstelle.
