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
