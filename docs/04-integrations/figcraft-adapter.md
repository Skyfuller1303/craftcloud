# FigCraft-Integration: Abhängigkeiten und Machbarkeitsgate

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
