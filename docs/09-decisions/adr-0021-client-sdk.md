# ADR-0021: Gemeinsamer Client und egui-Dateidialog

Status: In Umsetzung (AC-04), 2026-10-30.

## Entscheidung

Das SDK besteht aus einem I/O-freien, WASM-tauglichen `craftcloud-client`, einem nativen Adapter `craftcloud-client-native` und den hostgestalteten Widgets in `craftcloud-egui`. Alle verwenden den bestehenden OpenAPI-Vertrag, ohne Server-Crates einzubinden. Die Demo und der Roundtrip prüfen dieselben öffentlichen Schnittstellen.

Der native Adapter verwendet ureq 3 mit rustls und Plattform-Zertifikatsprüfung. Weiterleitungen werden nicht automatisch verfolgt. Nur HTTPS und HTTP auf Loopback sind zulässig. Zugriffstokens bleiben im Speicher; rotierende Erneuerungstokens werden ausschließlich über keyring 3 gespeichert. Ein fehlender Schlüsselbund führt zu einer Sitzung ohne Persistenz. Ein verlorener Refresh wird nicht wiederholt.

Die App schreibt zuerst lokal. Eine explizite Verknüpfung in `craftcloud-links.json` steuert nachfolgende Uploads mit der bekannten Basisversion. Fehler erhalten die lokale Datei. Konflikte verlangen eine Entscheidung und vor dem Überschreiben eine erneute Bestätigung. Es gibt keine Hintergrund-Warteschlange. Downloads werden vor Übergabe anhand von `Repr-Digest` geprüft. Nicht-FigCraft-Dateien verwenden die bestehenden fünfminütigen Leases mit einem Herzschlag alle 60 Sekunden.

Discovery ergänzt den bestehenden Endpunkt additiv. Das Formatregister beschreibt Endungen, interpretiert aber keine Inhalte. Der Client erkennt die `SECOND_FACTOR_REQUIRED`-Challenge und schließt sie mit `complete_second_factor` über `POST /api/v1/auth/device/login/second-factor` (AC-02) ab; die Demo und der egui-Anmeldedialog nehmen dafür einen Authenticator- oder Wiederherstellungscode entgegen. Die früher dokumentierte Abweichung „AC-02-Endpunkt fehlt“ entfällt; ein datenbankgestützter Test meldet ein Konto mit aktivem 2FA über das SDK an.

Geteilte Erneuerungstokens: Mehrere Clients oder Prozesse mit demselben Schlüsselbund-Eintrag lesen vor jeder Rotation den gespeicherten Wert und rotieren unter einer Sperre (Trait-Methoden `load` und `lock_exclusive`, Sperrdatei im App-Datenordner). Die Demo ersetzt bei jeder Verbindung den alten Client und gibt vorher dessen Leases frei.

## Abweichung bei der Mindestversion

Die Vorgaben egui 0.37 und Rust 1.90 sind nicht gemeinsam erfüllbar: Die egui/eframe-0.37.0-Manifeste verlangen Rust 1.95 (zuvor 0.36.2, ebenfalls 1.95). Die beiden SDK-Crates zielen weiterhin auf Rust 1.90 (ureq 3.5.0 verlangt 1.85, keyring 3.6.3 verlangt 1.75); Widget-Crate und Demo deklarieren ehrlich Rust 1.95. Seit 2026-10-30 folgt das SDK der Abhängigkeitslinie der Apps (egui/eframe 0.37.0, ureq =3.5.0, rfd 0.17.2), damit es per Pfadabhängigkeit eingebunden werden kann. Rust 1.90 ist lokal nicht installiert; dieser fehlende Build wird ausdrücklich berichtet. Das vorhandene WASM-Target wird gesondert geprüft.

## Offene Punkte

Die Verteilung an die Apps (privates Repository, Git-Abhängigkeit oder Veröffentlichung) bleibt eine Entscheidung des Projektverantwortlichen. Die VectorCraft-Endungen `svg`, `ai`, `vcraft` bedürfen der Bestätigung durch die Maintainer. Dieses ADR erlaubt keine Veröffentlichung.
