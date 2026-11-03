# ADR-0031: Versionsausdünnung

## Status

Implementiert und lokal mit unabhängiger Prüfung sowie vollständiger CI und PostgreSQL-Abnahme geprüft (2026-11-03). Veröffentlichung steht aus.

## Kontext

Vollkopien jeder Dateiversion lassen Speicher bei häufigen Speicherungen schnell wachsen. Die bisherige Altersaufbewahrung kennt keine Meilensteine und keine Versionsobergrenze je Datei.

## Entscheidung

Die Wartung behält die jüngsten Versionen innerhalb eines UTC-Stundenfensters, die jeweils jüngste Version je Kalendertag und je ISO-Woche in den konfigurierten Fenstern. Danach entfernt sie alle nicht ausgewählten Versionen; `retention.version_days` bleibt die Obergrenze des Aufbewahrungszeitraums. `versions.max_per_file` begrenzt zusätzlich nicht angeheftete Versionen, älteste zuerst; 0 bedeutet unbegrenzt. Kopf, gepinnte Versionen und FigCraft-Veröffentlichungen bleiben immer erhalten.

Sind sämtliche Rasterstufen und die Höchstzahl auf 0 gesetzt, bleibt der bisherige Altersscan erhalten. Dabei beginnt die Aufbewahrungszeit mit der Ablösung durch die nächste Versionsnummer. Die vollständig deaktivierte Ausdünnung verkürzt damit nicht unbeabsichtigt das bisherige Wiederherstellungsfenster.

Ein aktiver Upload kann wegen des bisherigen API-Vertrags keine Basisversion speichern. Bis ein eigener Basisversionsvertrag existiert, schützt ein offener Upload deshalb vorsorglich alle Versionen derselben Datei. Abgelaufene und geschlossene Uploads schützen nicht.

Verkürzte Werte wirken erst nach 24 Stunden. Umgebungsvariablen `CRAFTCLOUD_VERSIONS_KEEP_ALL_HOURS`, `CRAFTCLOUD_VERSIONS_DAILY_DAYS`, `CRAFTCLOUD_VERSIONS_WEEKLY_WEEKS` und `CRAFTCLOUD_VERSIONS_MAX_PER_FILE` haben Vorrang vor Instanzeinstellungen.

## Konsequenzen und Risiken

Die Zeitraster begrenzen die Zahl der nicht angehefteten Versionen bei häufigen Speicherungen. Angeheftete, veröffentlichte oder während offener Uploads vorsorglich geschützte Versionen können das Limit überschreiten. Löschen erfolgt ausschließlich über den bestehenden Versionslöschpfad; Blob-GC entfernt lokale und archivierte Objekte nachgelagert.

Eine offene Upload-Sitzung kann die Ausdünnung einer Datei bis zum Ablauf blockieren. Das verhindert, dass eine während des Uploads angenommene Basis vorzeitig gelöscht wird. Trockenläufe melden `versions_thinned_due`.

## Betrachtete Alternativen

- Eine Basisversion bei Upload-Erstellung speichern: verworfen, weil dies den Upload-API-Vertrag und SDK-Call-Sites erweitert.
- Nur die am Ende jedes Tages oder jeder Woche zuletzt angelegte Version zu schützen: verworfen, da die Datenbankuhr und UTC-Kalendergrenzen maßgeblich sein müssen.
- Eine parallele Archivlöschung einführen: verworfen; bestehende Blob-GC bearbeitet beide Speicherebenen.

## Review und Revision

Unabhängiges Datenverlust-Review vor Merge erforderlich.
