# CC-F15 — Gesundheitsprüfung, Telemetrie und Diagnose

> **Status:** lokal umgesetzt (PR-017, CC-P03-06; nicht geprüft, nicht gemergt; siehe „Umsetzungsstand“ am Ende). **Zielphase:** P3 (Admin & Operations). **Priorität:** MVP/Blocker.  
> **Abhängigkeiten:** Grundarchitektur (`docs/02-architecture/system.md`), Berechtigungsmatrix (`docs/05-security/permission-matrix.md`) und API-Konventionen (`docs/02-architecture/api-conventions.md`).

## Problem und Nutzen

Operatoren benötigen Liveness/Readiness, Storage-/DB-Status, Queue/Uploadzustände und sichere Request-Korrelation. Nutzerorientiertes Adminpanel zeigt Status je Komponente, Backupalter und verbleibenden Speicher. Keine Telemetrie muss an ArtCraft oder Dritte gesendet werden; Opt-in nur bei gesonderter Zustimmung.

## Detaillierter Funktionsumfang und technische Umsetzung

`GET /health/live` ohne Details, `/health/ready` kontrolliert DB/Storage mit angemessener Tiefe, `/metrics` nur intern/geschützt. Structured JSON Logs mit request_id, actor_id optional/pseudonym, org_id scoped; Redaction-Regeln für Cookies, URLs mit Tokens, Userdaten. Alerts für Storage fail, pending uploads, Backup age, DB saturation.

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

Ausfall DB führt zu readiness false; health endpoint ist nicht SSRF-/DB-Enumeration-Leck; metric endpoint nicht öffentlich; Log-Spam/PII; falsche Uhrzeit, dauernd abstürzender GC-Worker, Quota-Alarm.

## Prüfplan

- **Unit:** erfolgreiche Validierung, Grenzwerte, falsche Objektzustände; keine Panics, keine Unbounded Memory Paths.
- **Integration:** DB-Constraints und ggf. Objektstore verifizieren; persistente Wahrheit nach Server-Neustart.
- **Security:** Rollenwechsel und fremde Tenant-IDs auf jede betroffene Route; missbräuchliche Eingabewerte; Secrets-Redaction.
- **Recovery:** Fehler an DB/Storage/Netzwerk genau an Statusübergängen simulieren; einmalige und wiederholte Requests vergleichen.
- **UI/SDK:** verständliche 401/403/409/412/429/503-Nachrichten (je nach Funktion) und sichere lokale Fallbacks.

## Akzeptanzkriterien / Definition of Done

Cloud-Operator kann fehlerhafte Komponente identifizieren, ohne Kundendateien oder Tokens in Logs zu finden; Readiness signalisiert echten Fehler, aber Liveness verhindert Restart-Loops bei transienten DB-Ausfällen.

Zusätzlich: OpenAPI aktualisiert; aussagekräftige Tests erfolgreich; negative AuthZ-Tests; Migrations-/Rollback-Konsequenzen dokumentiert; Benutzerhilfe und Betriebshinweise aktualisiert; Maintainer-Review. Kein „fertig“-Status allein aufgrund eines Mockups.

## Nichtumfang und Abnahmefragen

Keine ungeprüfte Cross-App-/Cloud-Kopplung, keine proprietären Lizenzen kopieren, keine zusätzlichen Infrastruktur-Abhängigkeiten ohne ADR. Vor Merge prüfen: **API-Owner?** **Datenmigration?** **Offline-Verhalten?** **Echte Pilot-Validierung?**

## Umsetzungsstand (PR-017)

Lokal umgesetzt, nicht geprüft und nicht gemergt. Die Entscheidungen, die über dieses Briefing und ADR-0016 hinausgehen (Standardformat, Aufbau der Readiness-Antwort, getrennter Metrik-Listener, Datenträgerplatz über `df`), sind zur Aufnahme in das Decision-Log vorgeschlagen.

- **Logs.** `CRAFTCLOUD_LOG_FORMAT=json|text` (Standard `text`). Jede Anfrage erzeugt eine Zeile mit dem Ziel `craftcloud::request`: Anfrage-ID, Methode, **Routenvorlage** (zum Beispiel `/api/v1/files/{file_id}`, nie der konkrete Pfad; Anfragen ohne Route heißen `unmatched`), Status, `latency_ms`, `actor_id` und `org_id` (zufällige UUIDs, `-` wenn unbekannt). Die Zeile steht auf `info`, bei Status 5xx auf `warn`; Gesundheitsabfragen (`/health/*`) stehen immer auf `debug`, auch bei 5xx. Stattdessen wird der **Wechsel** des Readiness-Zustands einmal protokolliert (`warn` „readiness is failing“, `info` „readiness recovered“, Ziel `craftcloud::health`). Nie geloggt werden: Query-Strings, Fragmente, Header (Cookies, `Authorization`, CSRF-Token), Anfrage- und Antwortinhalte, E-Mail-Adressen, Anzeige-, Datei- und Projektnamen, Tokens. Das Audit-Ziel `craftcloud::audit` bleibt unverändert.
- **Liveness und Readiness.** `GET /health/live` bleibt ohne Prüfung (keine Neustart-Schleifen bei kurzem Datenbankausfall). `GET /health/ready` (öffentlich, für Reverse Proxy und Container-Healthcheck) prüft mit je höchstens 1,5 s (gesamte Prüfung höchstens 4 s, also unter dem Standard-Anfragetimeout): Datenbank antwortet auf eine Abfrage, Schema vollständig migriert, und, wenn ein Objektspeicher eingerichtet ist, eine Probe-Datei (`tmp/*.health`) lässt sich schreiben, lesen und löschen, und der Speicher gehört zu dieser Datenbank (Bindung). Die Prüfung läuft als einzelne eigene Aufgabe: gleichzeitige Abfragen warten auf sie, das Ergebnis wird 3 s wiederverwendet und auch dann gespeichert, wenn alle Aufrufer abbrechen (abgebrochene Abfragen können die Prüfungen nicht vervielfachen). Probe-Dateien, die ein Absturz übrig lässt, räumt der Wartungslauf nach einer Stunde weg. Nach dem Shutdown-Signal antwortet `/health/ready` sofort mit 503 (Draining), `live` bleibt 200. Antwort: `200 {"status":"ready"}`, `200 {"status":"degraded"}` (Epochen-Konflikt: Datenbank und Speicher passen zeitlich nicht zusammen, ein Restore wird vermutet; Auslieferung ist sicher, zerstörende Wartung ist blockiert) oder `503` mit dem Fehlercode `NOT_READY` in der üblichen Fehlerform. Ein Speicher, der beim Start als zu einer anderen Datenbank gehörig abgelehnt wurde (nur Upload und Download antworten 503 `STORAGE_UNAVAILABLE`), ist ebenfalls `degraded`, nicht 503: alles andere funktioniert weiter. Ein nicht beschreibbarer Speicher oder eine nicht erreichbare Datenbank bleibt 503. **Keine Komponentenangaben, Versionen, Zahlen oder Fehlertexte** in der öffentlichen Antwort.
- **Betreiberansicht.** `GET /api/v1/admin/health` (nur Instanzbetreiber, Aktion `HealthRead`, sonst 403; ohne Sitzung 401): Status je Komponente (Datenbank mit Pool `size/idle/in_use/max`, Migrationen, Speicher mit Bindung und freien Bytes des Datenträgers), offene und überfällige Upload-Sitzungen, letzter Wartungslauf (Zeit, Ergebnis, Fehlercodes) und Epochen-Konflikt, verworfene Audit-Ereignisse (Zähler dieses Prozesses, getrennt nach „lossy“ und „important“), Serverversion und Laufzeit, Backup-Alter (`null`, bis CC-P03-08 es füllt). Nur Zahlen, Flags und Codes. Die Admin-Oberfläche zeigt sie unter „System“ (Englisch und Deutsch, axe-Prüfung).
- **Metriken.** `GET /metrics` im Prometheus-Textformat, **nie auf dem öffentlichen Listener**. Aus: ohne `CRAFTCLOUD_METRICS_ADDR` wird nichts geöffnet. Mit `CRAFTCLOUD_METRICS_ADDR=<ip>:<port>` öffnet der Server einen eigenen Listener, der nur `GET /metrics` kennt. Ist die Adresse nicht Loopback, ist `CRAFTCLOUD_METRICS_TOKEN_FILE` Pflicht (Startfehler, Exit 2); mit der Datei verlangt der Listener `Authorization: Bearer <Token>` (Token mindestens 32 Zeichen, Vergleich in konstanter Zeit, falsch oder fehlend: 401 ohne Inhalt). Das Token ohne Adresse ist ein Konfigurationsfehler. Serien: `craftcloud_http_requests_total{route,method,status_class}`, `craftcloud_http_request_duration_seconds` (Histogramm je Route; gemessen bis zum Antwortkopf, nicht bis zum Ende eines Streams), `craftcloud_http_requests_in_flight` (bis der Antwortkopf erzeugt ist; ein gestreamter Body zählt nicht mehr mit), `craftcloud_readiness_checks_total`, `craftcloud_db_pool_connections`, `craftcloud_db_pool_idle_connections`, `craftcloud_db_pool_max_connections`, `craftcloud_upload_sessions_open`, `craftcloud_upload_bytes_received_total`, `craftcloud_download_bytes_sent_total`, `craftcloud_maintenance_runs_total{result}`, `craftcloud_maintenance_last_success_timestamp_seconds`, `craftcloud_maintenance_last_run_timestamp_seconds`, `craftcloud_audit_events_dropped_total{class}`, `craftcloud_rate_limit_rejections_total{class}`, `craftcloud_storage_free_bytes`, `craftcloud_storage_epoch_conflict`, `craftcloud_ready`, `craftcloud_uptime_seconds`, `craftcloud_build_info{version}`. Keine Benutzer- oder Organisations-IDs als Label (Kardinalität und Datenschutz); Routenlabels stammen aus einer festen Tabelle, ein Client kann keine neuen Werte erzeugen.
- **Nicht enthalten / offen.** Freier Platz wird über `df -Pk` gemessen (die Standardbibliothek hat keinen Aufruf, `unsafe` ist verboten; eine Messung zugleich, Ergebnis 30 s zwischengespeichert, auch „unbekannt“; hängt `df`, wird es nach 1,5 s beendet und im Hintergrund eingesammelt): ohne `df` im Image, unter Windows, bei Leerzeichen im Dateisystem- oder Einhängenamen (Zeile hat nicht genau sechs Spalten) ist der Wert `null` bzw. die Serie fehlt. `CRAFTCLOUD_METRICS_ADDR` darf mit `CRAFTCLOUD_BIND` weder dieselbe Adresse noch denselben Port bei einer unspezifizierten Adresse (`0.0.0.0`, `::`) teilen (Startfehler). Zähler (Wartungsläufe, Audit-Drops, Ratenlimit) gelten je Prozess und beginnen nach einem Neustart bei 0. Keine Alarmregeln und keine Dashboards (nur die Serien), kein Backup-Alter (CC-P03-08), keine Traces, keine Telemetrie an Dritte.
