# ADR-0014: Standardgrenzen für Dateiablage in P2

- **Status:** Accepted für den unabhängigen lokalen Prototyp (Projektverantwortlicher, 2026-10-28); bei Übergabe an Maintainer erneut zu prüfen
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps

## Kontext

CC-F07 und CC-F10 verlangen begrenzte Uploads, Quoten und ein Wiederherstellungsfenster, legen aber keine Standardwerte fest. Die Werte müssen vor PR-008 bis PR-011 feststehen, damit Konfiguration, Tests und Doku übereinstimmen.

## Entscheidung

1. **Maximale Dateigröße:** Standard 5 GiB pro Datei, vom Operator konfigurierbar. Upload und Download streamen mit begrenztem Arbeitsspeicher; Tests prüfen mindestens 1 GiB.
2. **Quota:** Neue Organisationen haben standardmäßig keine Quota (unbegrenzt, die Festplatte ist die Grenze). Der Operator kann Quoten pro Organisation und Workspace setzen. Die Durchsetzung über `reserved + committed` wird trotzdem vollständig gebaut und getestet.
3. **Aufbewahrung:** Soft-gelöschte Dateien und ältere Versionen bleiben standardmäßig 30 Tage wiederherstellbar, danach entfernt die Garbage Collection sie endgültig. Der Wert ist konfigurierbar. Backups können Daten länger enthalten; das steht in den Datenschutzhinweisen.

## Konsequenzen und Risiken

Ohne Standard-Quota kann eine einzelne Organisation die Festplatte füllen; Betreiber müssen Quoten bewusst setzen oder den Speicher überwachen (CC-F15). Uploads bis 5 GiB ohne Wiederaufnahme können bei instabilen Verbindungen wiederholt abbrechen; resumable Uploads sind ein späterer Ausbau.

## Betrachtete Alternativen

1 GiB oder 20 GiB Dateigröße; 100 GiB Standard-Quota; 7 oder 90 Tage Aufbewahrung.

## Review und Revision

Nach den ersten Lasttests in P2 und vor einer Übergabe an Maintainer prüfen.
