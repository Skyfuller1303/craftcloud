# ADR-0020: Grants und Bearbeitungssperren

- Status: angenommen für AC-03
- Datum: 2026-10-30

## Kontext

Workspace-Rollen allein gewähren zu breite Freigaben. Gleichzeitig benötigen Craft-Apps eine zeitlich begrenzte Bearbeitungslease, damit paralleles Speichern nicht stillschweigend kollidiert. Jede Entscheidung muss in der API und Datenbank gegen aktuelle Mitgliedschaft, Grants, Objektzustand und Sitzung geprüft werden.

## Entscheidungen

- Grants binden Organisation, Workspace, Zielart, Ziel-ID, empfangende aktive Organisationsmitgliedschaft und Rolle `viewer` oder `editor`. Composite-Fremdschlüssel verhindern organisationsübergreifende Verknüpfungen. Aktive Duplikate je Ziel und Empfänger sind ausgeschlossen. Widerruf bleibt auditierbar.
- `guest` ist eine Organisationsrolle ohne impliziten Workspace-Zugriff. Workspace-bezogene Einladung speichert den Workspace-Kontext; Annahme erzeugt ausschließlich die Gastmitgliedschaft, keine Workspace-Rolle.
- Objektlese- und Schreibrechte sind das Maximum aus Workspace-Rolle und Datei-/Projekt-Grants. Grants enden bei `editor`; sie geben keine Workspace-Verwaltung, Mitgliederverwaltung, Projektanlage, Projektumbenennung oder Freigabe-Verwaltung. Nur Workspace-Editoren und höher verwalten Grants. Berechtigungsauflösung bleibt serverseitig und pro Anfrage frisch.
- Ein Datei-Grant gewährt keine Projektlese- oder Listenrechte. Grant-only Dateizugriff liefert nur nötige Parent-Projekt-Metadaten im Datei- beziehungsweise `shared-with-me`-DTO. Ein Projekt-Grant erlaubt die Projektansicht und Dateiaktionen im Projekt, aber keine Projektanlage, Umbenennung oder Workspace-Verwaltung. Gemeinsame Listen verwenden tenantgebundene Keyset-Abfragen.
- Grant-Rollenänderungen verlangen einen versionsgebundenen ETag `"grant-v<n>"`; fehlendes `If-Match` ist 428, veraltetes oder syntaktisch ungültiges 412 `VERSION_CONFLICT` mit aktuellem ETag. Dadurch werden parallele Dialogänderungen nicht still überschrieben.
- Dateisperren sind Leases mit fünf Minuten Laufzeit und 60-Sekunden-Heartbeat. Sie gehören zu einer aktiven authentifizierten Sitzung; Client-IDs sind nur beschriftete Eingaben, keine Geräteidentität. Ablauf macht die Lease unwirksam, Bereinigung entfernt alte Zeilen.
- Eine zweite Sitzung derselben Person darf nur mit explizitem `takeover=true` übernehmen; Workspace-Admins und höhere Rollen dürfen fremde Sperren explizit erzwingen. Beide Vorgänge werden auditiert. Herzschläge erzeugen kein Audit-Ereignis.
- FigCraft-Dateien unterstützen keine Sperre. Fremde aktive Sperren blockieren Datei-Metadatenänderungen, Upload-Anlage und -Abschluss, Versionsabschluss/-Restore sowie Datei-Papierkorb und -Restore; Lesen und Download bleiben erlaubt. Projekt-Papierkorb und -Restore prüfen ebenfalls alle aktiven Sperren der enthaltenen Dateien. Sperren werden bei Grant-Widerruf, Workspace-Rollenentzug oder Mitgliedschaftsentzug in derselben Organisationstransaktion entfernt.
- Eigene Upload-Statusabfrage und eigener Abbruch folgen der aktuellen Sichtbarkeit der Quelldatei; ein File-Grant reicht. Upload-Anlage und Inhaltsabschluss verlangen weiterhin Editor-Rechte. Fremde Upload-Sitzungen bleiben fuer grant-only Actor verborgen.
- Membership-, Einladung-, Grant- und Lock-Mutationen serialisieren unter dem Organisations-Lock. Sensible Änderungen und Audit-Ereignisse teilen dieselbe Transaktion. Upload-Abschluss autorisiert unter demselben Organisations-Lock erneut.
- Backups sichern Grants; flüchtige Locks werden weder gesichert noch restauriert.

## Folgen

- Migration 0016 ergänzt `guest`, Grants, Sperren und nötige Indizes/Constraints. Bestehende Einladungen bleiben kompatibel.
- Jede Objekt- und Schreibroute muss Grants beziehungsweise Leases berücksichtigen; Integrationstests decken Scope, parallele Lease-Anlage, Entzug während Upload und alle Schreibwege ab.
- Geräte-Sitzungen werden erst integriert, wenn AC-01 ihr persistentes Sitzungsmodell bereitstellt. AC-03 darf Browser-Sitzungen real binden, aber keine Geräteidentität aus Clientdaten vortäuschen.
- Die administrative Oberfläche kann nur die vom Server gelieferten `can_share`, `can_force_lock` und Lock-/Grant-Daten als Hinweise nutzen; API-Policy bleibt Autorität.

## Verworfene Alternativen

- Öffentliche oder organisationsübergreifende Links: außerhalb AC-03 und erhöhte Angriffsfläche.
- Weitergabe durch Grant-Empfänger: unnötige Rechteausweitung und unklare Verantwortlichkeit.
- Dauerhafte Checkout-Sperre: kann nach Client-Absturz unbegrenzt blockieren.
- Clientseitig behauptete Geräte-ID: nicht authentifiziert und daher keine Sitzungsidentität.
- Workspace-Mitgliedschaft für Gäste: verrät und erschließt unnötig den gesamten Workspace.
