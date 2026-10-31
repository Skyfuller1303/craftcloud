# ADR-0027: Plätze und App-Zuweisung

Datum: 2026-10-31. Status: lokale Umsetzung; nicht veröffentlicht.

## Kontext

CraftCloud-Organisationen brauchen eine optionale Obergrenze für aktive Personen und eine serverseitige Zuweisung von Craft-Apps. Die Zuweisung steuert ausschließlich Cloudzugriff; die Apps bleiben lokal nutzbar. Browser-Sitzungen dürfen dadurch nicht eingeschränkt werden.

## Entscheidungen

1. `organizations.seat_limit` ist optional. Ein Platz zählt für aktive, nicht deaktivierte Mitglieder außer Gästen. `app_access_mode` ist `all` oder `assigned`; Altbestand und neue Organisationen bleiben mit `all` kompatibel.
2. `member_app_access` speichert explizite Zuordnungen je Organisation, Person und App. Owner und Admin behalten alle Apps; Gäste erhalten keine Zuordnung.
3. Die API verwendet `GET /organizations/{org_id}/apps`, `PATCH /organizations/{org_id}/app-access`, `PUT /organizations/{org_id}/members/{user_id}/apps` und `PATCH /admin/organizations/{org_id}/seats`. Mengenaktionen bleiben einzelne Requests je Person.
4. Der Server enthält die Apps `figcraft`, `photocraft` und `vectorcraft` mit ihren Formaten. `CRAFTCLOUD_APP_CATALOG_FILE` kann weitere Apps ergänzen; die Datei ist auf 256 KiB begrenzt. Jede nicht auskommentierte UTF-8-Zeile hat das Format `id|name|format,format`; leere Zeilen und `#`-Kommentare werden ignoriert. IDs und Formate müssen gültig und eindeutig sein. Kollisionen mit eingebauten IDs und mehr als 64 Apps insgesamt lassen den Start mit Konfigurationsfehler scheitern. Unbekannte Kennungen werden beim Zuweisen abgelehnt.
5. App-Gerätelogin wird nach dem zweiten Faktor gegen alle aktiven Mitgliedschaften geprüft: mindestens eine Organisation muss die App erlauben. Im Standardmodus `all` bleiben unbekannte `client_id`-Werte für Rückwärtskompatibilität zulässig. Im Modus `assigned` muss die App-ID im Serverkatalog stehen und in mindestens einer aktiven Organisation erlaubt sein. Launcher- und CLI-Geräte sowie Browser-Sitzungen bleiben von der App-Zuweisung unberührt. Geschützte Datei-, Versions-, Upload-, Sperr- und FigCraft-Aktionen prüfen die Zuweisung erneut im jeweiligen Organisationskontext. Eine Organisation im Modus `all` erlaubt den kontoweiten App-Login; eingeschränkte Organisationen behalten ihre Anfrageprüfung.
6. Das Platzlimit zählt aktive, nicht deaktivierte Mitglieder außer Gästen. Ausstehende Einladungen für Nichtgäste reservieren Plätze. Einladung, Annahme, Mitglied-Add, Gast-Hochstufung und Reaktivierung prüfen unter der Organisationssperre. Dadurch überschreiten parallele admissions das Limit nicht. Ein Limit darf unter die aktuelle Belegung gesenkt werden; bestehende Mitglieder bleiben aktiv und neue Zulassungen scheitern bis die Belegung sinkt.
7. Globale Kontoreaktivierung ermittelt aktive Mitgliedschaften, sperrt die Organisationen in stabiler UUID-Reihenfolge und danach das Konto. Sie validiert die Mitgliedschaftsmenge nach dem Kontoschloss erneut. Bei Änderung startet die Transaktion begrenzt neu; nach vier Konflikten kommt 409 `MEMBERSHIP_STATE_CHANGED`, ohne Kontoänderung. Ein Limitkonflikt liefert 409 `SEAT_LIMIT_REACHED`.

## Folgen und offene Punkte

Migration 0020 fügt die Konfiguration und Zuordnungstabelle hinzu. API und Datenbank setzen Zuweisung, Sitzungsentzug, Platzreservierung und Limits durch. Der App-Katalog startet mit stabilen eingebauten Apps und kann über die genannte Datei erweitert werden. Der OpenAPI-Snapshot wurde aktualisiert. Backups enthalten ab Schema 20 die Tabelle `member_app_access` sowie die Herkunftsfelder der FigCraft-Links; ältere Backups werden unverändert wiederhergestellt und danach migriert. Die im Bereich „Apps“ gezeigte Speicherzahl ist die logische Größe aller aufbewahrten Versionen der Dateien einer App einschließlich Papierkorb, also dieselbe Grundlage wie das Speicherkontingent.
