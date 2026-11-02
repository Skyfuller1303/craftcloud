# ADR-0028: Verschlüsselte Speicherstufen

- **Status:** Proposed; Umsetzung und unabhängige Sicherheitsprüfung laufen. Die Archivfunktion ist bis zur abgeschlossenen Prüfung beim Serverstart gesperrt.
- **Datum:** 2026-10-31

## Kontext

AC-12 hält aktuelle Dateiversionen lokal und archiviert ältere Versionen optional bei einem externen Anbieter. Ohne Archivkonfiguration bleibt der bisherige Speicherpfad erhalten.

## Entscheidung

Archivdaten verlassen den Server ausschließlich blockweise authentifiziert verschlüsselt. Ein eigener aus dem Serverschlüssel abgeleiteter Archivschlüssel schützt zufällige Objektschlüssel. Archivierung veröffentlicht und prüft die externe Kopie vor dem transaktionalen Zustandswechsel und löscht die lokale Kopie erst danach. Rückholung und Wiederherstellung benötigen eine verifizierte lokale Kopie. Head-Versionen und laufende Uploads bleiben lokal. Externe Speicher nutzen opake Objektnamen und erhalten keine Dateinamen oder Organisationsmetadaten.

## Alternativen und Risiken

Ein eingebundenes Netzlaufwerk trennt die Stufen nicht zuverlässig und kann bei Netzfehlern lokale Schreibwege blockieren. Unverschlüssertes externes Archiv wurde wegen der zusätzlichen Vertrauensgrenze verworfen. Datenverlust, Wiederanlauf nach Absturz, Schlüsselverlust, parallele Wartung und Backup/Restore erfordern eigene Tests und unabhängigen Sicherheitsreview. Der konkrete Stand und verbleibende Grenzen werden vor Abschluss ergänzt.

## Folgeauftrag

Die laufende Nacharbeit erweitert die Archivprüfung um ein konfigurierbares Byte- und Zeitbudget sowie dauerhaften blockweisen Fortschritt. Claims sollen die Prüfung gegen parallele Garbage Collection und Rückholung absichern. Konfigurationswechsel, Rückholungen und Freigaben von Backup-Pins sollen nachvollziehbar auditiert werden. Die Archivfunktion bleibt gesperrt, bis die geforderten Absturz-, Provider- und Ende-zu-Ende-Nachweise sowie die Sicherheitsprüfung abgeschlossen sind. Der konkrete Prüfstand steht im Folgebericht.
