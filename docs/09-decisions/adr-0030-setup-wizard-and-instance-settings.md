# ADR-0030: Einrichtungsassistent und Instanzeinstellungen

- **Status:** Vorgeschlagen; Umsetzung und unabhängige Sicherheitsprüfung laufen.
- **Datum:** 2026-11-02

## Kontext

Die einmalige Initialisierung erstellt bereits das erste Betreiberkonto und die erste Organisation. Weitere Betriebsschritte sollen unterbrochen und von berechtigten Betreibern fortgesetzt werden können, ohne die einmalige Bootstrap-Grenze aufzuweichen.

## Entscheidung

Nach der einmaligen Kontoerstellung ist der Assistent ausschließlich für angemeldete Instanzbetreiber zugänglich. Sein Fortschritt wird serverseitig gespeichert. Konfigurationswerte für Mail und Aufbewahrung können in der Datenbank verwaltet werden; ausdrücklich gesetzte Umgebungskonfiguration hat Vorrang und bleibt in der Oberfläche schreibgeschützt. Infrastruktur- und Archivkonfiguration bleiben außerhalb des Assistenten.

Gespeicherte Zugangsdaten werden mit dem bestehenden Serverschlüssel authentifiziert verschlüsselt und niemals über die API zurückgegeben. Änderungen an Geheimnissen benötigen eine nachweisbar frische Authentifizierung und gegebenenfalls einen zweiten Faktor. Backup und Wiederherstellung müssen die neuen Daten und die Abhängigkeit vom Serverschlüssel berücksichtigen.

## Alternativen und Risiken

Nur lokal im Browser gespeicherter Fortschritt würde beim Gerätewechsel verloren gehen. Eine generische Konfigurationsoberfläche für Infrastruktur und Archiv würde zusätzliche Geheimnisse und Betriebsgrenzen offenlegen. Mail-Neukonfiguration, Umgebungspriorität, parallele Operatoraktionen, Schlüsselrotation, Vertrauensgrenzen bei Systemprüfungen und Wiederherstellung benötigen gezielte Tests und eine unabhängige Sicherheitsprüfung. Der konkrete Prüfstand wird im AC-13-Bericht festgehalten.

## Überarbeitung nach dem Review

Die unabhängige Prüfung ergab sieben Änderungen, die in diese Entscheidung eingehen.

1. **Zielwechsel löscht Geheimnisse.** Ändert ein Request `mail.transport`, `mail.host`, `mail.port`, `mail.tls`, `mail.tenant_id` oder `mail.client_id`, löscht die Transaktion alle gespeicherten Mailgeheimnisse, die der Request nicht selbst neu setzt, und auditiert das Löschen ohne Werte. Alternativ wäre eine Bindung jedes Geheimnisses an das Ziel möglich gewesen; das Löschen ist einfacher und fehlerärmer. Die Oberfläche warnt vorab und danach.
2. **Umgebungsvorrang ist vollständig.** Umgebungs-Mail sperrt alle Mailwerte einschließlich aller Geheimnisschlüssel und liest keine Datenbankwerte.
3. **Verkürzte Fristen.** Frische Anmeldung, Bestätigungsdialog und 24 Stunden Karenz mit der längeren der beiden Fristen. Die alte Frist liegt als `previous` im JSON-Wert der Einstellung; `updated_at` bestimmt das Ende der Karenz. Eine eigene Spalte wäre nur für diese vier Schlüssel nötig gewesen und wurde verworfen.
4. **Ungültige Fristen stoppen die Wartung nicht.** Ungültige Werte werden ignoriert und mit Schlüsselname geloggt; zusätzlich prüft ein CHECK in Migration 0023 den Wertebereich.
5. **Klartext für Geheimnisschlüssel** wird mit `422` abgelehnt.
6. **Fail-fast** bei Umgebungs-Mail ohne gültigen Serverschlüssel wie vor AC-13.
7. **Upgrade.** Migration 0023 markiert für bestehende Installationen alle Schritte als `skipped` und `summary` als `done`. Das gilt auch für Wiederherstellungen eines Backups von vor 0023, weil die Migration danach läuft.

Offen bleibt das Mehrbetreibermodell (siehe oben).
