# ADR-0012: PostgreSQL-Zugriff, Migrationen und Datenbank-Testinfrastruktur

- **Status:** Proposed (unabhängiger lokaler Prototyp; bei Übergabe an Maintainer erneut zu prüfen)
- **Datum:** 2026-10-28
- **Owner:** Projektverantwortlicher des privaten Prototyps

## Kontext

PR-003 (CC-P01-03) führt das erste Datenbankschema, ein Migrationssystem und eine mandantenbezogene Abfrageschicht ein. ADR-0003 legt PostgreSQL als Zieldatenbank fest; ADR-0011 hielt `CRAFTCLOUD_DATABASE_URL` als reservierten Schlüssel zurück.

## Entscheidung

1. **sqlx 0.8 mit Laufzeit-Abfragen** (`sqlx::query`, `query_as`), Features `runtime-tokio`, `tls-rustls-ring-webpki`, `postgres`, `migrate`, `macros` (nur für `sqlx::migrate!` und `FromRow`), `uuid`, `time`. Keine `query!`-Makros mit Compile-Zeit-Prüfung: Der Build braucht keine Datenbank. Nur PostgreSQL. Zeitwerte mit der `time`-Crate (kleiner als `chrono`, kein Legacy-Zeitzonenballast; Datenbanktyp `timestamptz`, UTC).
2. **Nur vorwärts gerichtete Migrationen**, eingebettet aus `crates/craftcloud-db/migrations/`, ohne Down-Dateien. Rollback bedeutet Backup zurückspielen. Jede Datei dokumentiert Zweck, Tenant-Scope und Restore-Hinweis.
3. **Migrationen laufen nie implizit.** `craftcloud-server migrate` wendet ausstehende Migrationen unter einem Advisory Lock an und meldet die angewendeten Versionen; nebenläufige Läufer werden serialisiert und jede Migration wird genau einmal angewendet. Der normale Start verbindet sich (falls konfiguriert), prüft, dass alle eingebetteten Migrationen mit identischer Prüfsumme angewendet sind und keine unbekannten oder fehlgeschlagenen vorliegen, und endet sonst mit Exit-Code 1 und dem Hinweis auf `craftcloud-server migrate`. Ohne konfigurierte Datenbank läuft der Server im Prototypmodus mit Warnung.
4. **Datenbank-URL nur über Datei.** `CRAFTCLOUD_DATABASE_URL_FILE` verweist auf eine Secret-Datei (Regeln aus ADR-0011); `CRAFTCLOUD_DATABASE_URL` bleibt abgelehnt, weil URLs Passwörter enthalten. Die URL wird nie geloggt und nie in Fehlermeldungen wiederholt. Für entfernte Datenbanken wird `sslmode=verify-full` empfohlen. Pool: `CRAFTCLOUD_DATABASE_MAX_CONNECTIONS` (1 bis 100, Standard 10), `CRAFTCLOUD_DATABASE_CONNECT_TIMEOUT_SECS` (1 bis 60, Standard 5).
5. **IDs und Löschverhalten.** UUID v4 wird in der Anwendung erzeugt (keine Datenbank-Erweiterung nötig). Alle Fremdschlüssel sind `ON DELETE RESTRICT`; Soft-Delete folgt später, keine kaskadierenden Hartlöschungen. Tenant-gebundene Tabellen tragen `org_id`; Zugehörigkeit zur gleichen Organisation wird per zusammengesetzten Fremdschlüsseln erzwungen. PostgreSQL meldet RESTRICT-Verletzungen mit SQLSTATE 23001; `DbError` ordnet sie wie 23503 als `ForeignKeyViolation` ein.
6. **Mandantenbezogene Abfrageschicht.** Repository-Funktionen nehmen `&OrgScope` und filtern in SQL immer nach `org_id`; ein Objekt einer anderen Organisation ist von einem fehlenden nicht unterscheidbar (`None`). `OrgScope::caller_has_authorized` führt selbst keine Autorisierung durch und darf erst nach einer Prüfung durch die Policy-Schicht (PR-006) aufgerufen werden. *(Nachtrag 2026-10-29, PR-006: Dieser Konstruktor wurde entfernt; `OrgScope` entsteht nur noch über `OrgScope::from_permit` aus einer positiven Policy-Entscheidung.)* Listen haben eine feste Obergrenze (200). `DbError` liefert getypte Varianten für Unique-, Fremdschlüssel- und Check-Verletzungen; rohe Treiber- oder SQL-Texte verlassen die Crate nicht.
7. **Testinfrastruktur.** Integrationstests laufen gegen eine echte PostgreSQL-Instanz (`#[ignore]`, `cargo xtask db-test`): Der Befehl startet einen Docker-Container mit exakt gepinntem Image (`postgres:18.6`, Digest wird protokolliert), an `127.0.0.1` auf einem freien Port gebunden, mit `--memory 1g --cpus 2`, zufälligem Passwort pro Lauf, und entfernt ihn immer. Jeder Test legt eine eigene Datenbank `cc_test_<uuid>` an und löscht sie. `cargo xtask ci` benötigt weder Docker noch eine Datenbank. Speicherbudget (Nutzergrenze 16 GiB für alle Agentenläufe): Container 1 GiB, Windows-Cargo-Läufe höchstens 13 GiB, solange Docker Desktop läuft.

## Konsequenzen und Risiken

Die Docker-VM (WSL2) kann bis zur `.wslconfig`-Grenze wachsen; das Container-Limit begrenzt nur den Test-Container. Ohne Compile-Zeit-Prüfung werden SQL-Fehler erst in den Datenbanktests sichtbar. `time`, `sqlx` und `rustls` vergrößern den Abhängigkeitsbaum erheblich (MSRV 1.99 entspricht der lokalen Toolchain und wird bei jedem Build mitgeprüft). Die Prüfsummen-Strenge macht nachträgliche Änderungen an angewendeten Migrationen unmöglich; Korrekturen erfordern eine neue Migration.

## Betrachtete Alternativen

`query!`-Makros (benötigen Datenbank oder `.sqlx`-Cache zur Build-Zeit); Diesel oder SeaORM (mehr Abstraktion, kein Gewinn für ein kleines Schema); implizite Migration beim Start (riskant bei mehreren Instanzen und ohne Backup); Down-Migrationen (trügerische Rollback-Sicherheit); `testcontainers` (zusätzliche Abhängigkeit statt kleinem xtask-Befehl).

## Review und Revision

Zusammen mit ADR-0003 und ADR-0011 prüfen. Offen: TLS-Wurzelzertifikate (derzeit webpki-Roots; private CAs über `sslrootcert`), Postgres-Major-Upgrade-Strategie, Zeitpunkt für Row-Level-Security als zusätzliche Verteidigungslinie.
