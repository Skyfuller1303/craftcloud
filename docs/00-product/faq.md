# Häufige Fragen zur vorgeschlagenen CraftCloud

**Muss ich mich beim Start des Launchers anmelden?** Nein. Cloud ist opt-in. App startet lokal unverändert.

**Kann ich CraftCloud auf NAS/VPS hosten?** Ziel ja, aber ein belastbarer Docker-/Backup-/Sicherheitsnachweis ist erst Teil von P3. Ein NAS als SMB-Volume muss eigene fsync/locking Tests bestehen.

**Kann ich einfach das bestehende ArtCraft Backend nutzen?** Architekturideen ja, aber die gehostete KI-Plattform hat andere Funktionen und eine gesonderte WIP-Lizenz. Keine unautorisierte Übernahme.

**Ist eine `.figcraft` Cloud-Datei automatisch live kollaborativ?** Nein. Cloud Versioning speichert opake Dateien. FigCraft Live-Collaboration erfolgt über `figcraft-server` und braucht gesonderten Auth-Adapter.

**Sind Dateien Ende-zu-Ende verschlüsselt?** Im MVP ausdrücklich nein. TLS transit; Betreiber kann im Regelfall ruhende Daten lesen. E2EE wäre ein separates Design mit Tradeoffs.

**Ist die ZIP eine fertige GitHub-Repo?** Nein. Es ist eine referenzierbare **Dokumentations-/Projektstruktur**, die eine spätere echte Repo vorbereitet. Weder Backend noch Frontend noch CI-Workflows wurden implementiert oder getestet.

**Wieso React/TS, wenn der Launcher Rust-only ist?** CraftCloud-Admin ist unabhängige Browser-App. Der Launcher selbst bleibt Rust-only und erhält später höchstens eine optionale Rust-seitige Verbindungskonfiguration.

**Warum PostgreSQL statt SQLite?** Die vorläufige MVP-Architektur priorisiert Concurrent Writes, CAS und adminfreundliche Backups. SQLite ist denkbar, sollte aber separat auf Concurrency/Recovery geprüft werden.
