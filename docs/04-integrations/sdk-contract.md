# Client-SDK: API und Integration

[AC-04](../12-app-cloud/ac-04-client-sdk-and-picker.md) stellt drei Crates bereit: `craftcloud-client` enthält I/O-freie Abläufe und handgeschriebene OpenAPI-DTOs, `craftcloud-client-native` übernimmt HTTP/TLS, Schlüsselbund und Dateien, `craftcloud-egui` liefert hostgestaltete Widgets. Keine dieser Crates hängt von Server-Crates ab. [ADR-0021](../09-decisions/adr-0021-client-sdk.md) beschreibt die Entscheidungen.

## Öffentliche API

`CloudTransport::send(Request) -> Result<Response, CloudError>` ist die synchrone Transportgrenze. `CloudClient<T>` besitzt Server-URL und Sitzung. `UreqTransport` und alle Dateioperationen gehören auf einen Worker, nicht auf den egui-Thread.

```rust,no_run
use craftcloud_client::CloudClient;
use craftcloud_client_native::UreqTransport;

# fn example() -> Result<(), craftcloud_client::CloudError> {
let client = CloudClient::new("https://cloud.example.test", UreqTransport::default())?;
let discovery = client.discover()?;
assert_eq!(discovery.api_version.split('.').next(), Some("1"));
# Ok(())
# }
```

`discover` prüft die API-Hauptversion. `login`, `refresh`, `restore_refresh_token`, `refresh_token`, `clear_tokens` und `logout` verwalten die Geräte-Sitzung. `get<T>` und `post<T, B>` verwenden denselben Authentifizierungspfad für die Organisations-, Workspace-, Projekt-, Datei- und Freigabe-DTOs. Listen werden mit `next_cursor` weitergeblättert.

`create_upload`, `upload_content` und `commit_version` machen die Upload-Schritte einzeln nutzbar. `PendingUpload` erhält die Upload-ID und den Idempotency-Key für einen weiteren Commit nach 412. `upload` fasst den gewöhnlichen Ablauf zusammen; `upload_with_cancel` nimmt ein geteiltes Abbruchsignal entgegen. Initiale Versionen verwenden `If-None-Match: *`, spätere Versionen `If-Match: "version:<basis>"`.

`download(file_id, Some(version_id))` lädt die unveränderliche Version und prüft `Repr-Digest` vor Rückgabe der Bytes. Nach dem Lesen von Datei-Metadaten muss der Host deren konkrete Kopfversion übergeben, damit zwischenzeitliche Commits nicht zu einer falschen lokalen Basis führen. `acquire_lock` verlängert zugleich die Lease; `release_lock` gibt sie frei. FigCraft-Dateien benötigen keine Lease.

## Lokale Dateien

`LinksStore::new(app_data_dir)` verwaltet `craftcloud-links.json`: lokaler Pfad, Server, Datei-ID, Basisversion und SHA-256. Zugangsdaten gehören nicht in diese Datei. Lokales Speichern geschieht zuerst. Ohne passende Verknüpfung bleibt es lokal; ein Cloud-Fehler erhält die lokalen Bytes und die bekannte Basis. Ein erneuter Upload ist eine ausdrückliche Aktion, keine Hintergrund-Warteschlange.

`VersionCache` erhält einen Stammordner namens `CraftCloud`; darunter folgen Server-Hash, Datei-ID und Versionsdatei. Pfadsegmente werden geprüft. Bereits vorhandene abweichende Bytes dürfen nicht überschrieben werden, weil die Gast-App die Cache-Datei inzwischen bearbeitet haben kann.

`FileLeaseHeartbeat` bleibt auf dem Worker aktiv, solange die App die Datei als offen meldet. Er erneuert alle 60 Sekunden und gibt beim Schließen frei. Der Host muss Heartbeat-Fehler als Verlust der Bearbeitungserlaubnis behandeln. Der Server prüft weiterhin jede Schreiboperation.

Bei 423 stehen Abbruch und schreibgeschütztes Öffnen zur Wahl. Bei 412 bleibt die lokale Datei erhalten. „Beide behalten“ erstellt eine neue Datei mit Person und Datum im Namen. „Überschreiben“ benötigt eine erneute Bestätigung und einen Commit gegen die aktuell gelesene Kopfversion; ein weiterer Konflikt wird erneut angezeigt.

## Authentifizierung und TLS

Zugriffstokens bleiben im Speicher. `KeychainTokenStore` trennt Erneuerungstokens nach Server, Konto und App. Der Host speichert den nach jeder Rotation erhaltenen Wert und entfernt den Eintrag bei Abmeldung oder unbrauchbarer Sitzung. Ohne Schlüsselbund gibt es keine Persistenz; Klartextdateien sind kein Fallback. Credentialtragende Debug-Ausgaben sind maskiert.

401 führt höchstens zu einer kontrollierten Erneuerung und Wiederholung der ursprünglichen Anfrage. Eine verlorene Refresh-Antwort wird nicht wiederholt, weil der Server ohne Gnadenfrist rotiert. `SECOND_FACTOR_REQUIRED` erhält `details.challenge_id`. AC-01 definiert keinen Endpunkt zum Abschließen der Challenge; diese Verbindung hängt vom parallel entwickelten AC-02-Vertrag ab.

HTTPS ist verpflichtend, außer HTTP auf Loopback. Benutzerinformationen in der URL sind verboten. ureq verwendet rustls mit `RootCerts::PlatformVerifier` und verfolgt keine Weiterleitungen. Die Zertifikatsprüfung ist nicht abschaltbar; private Unternehmens-CAs werden über den Plattform-Zertifikatsspeicher eingerichtet.

## Widgets und Beispiele

`PickerState::show` liefert `Action`-Werte. `Translator` bindet das i18n-System der Gast-App ein; `DefaultTranslator(Language::English/German)` liefert Vorgaben. Gestaltung und Farben stammen aus dem egui-Kontext. Konflikt, Überschreibbestätigung, Sperre, Fortschritt und Offline-Hinweis sind eigene Zustände. Die Demo verarbeitet Aktionen auf einem Worker und verwirft überholte Ereignisse.

`crates/craftcloud-client-native/examples/roundtrip.rs` verwendet `CC_SERVER_URL`, `CC_EMAIL`, `CC_PASSWORD`, `CC_PROJECT_ID` sowie einen lokalen Dateipfad als Argument. Zugangsdaten werden nicht ausgegeben. Die Demo verwendet `CRAFTCLOUD_DEMO_SERVER`; synthetische Abnahmedaten können über `CRAFTCLOUD_DEMO_EMAIL` und `CRAFTCLOUD_DEMO_PASSWORD` übergeben werden. `CRAFTCLOUD_DEMO_DATA_DIR` isoliert ihre lokalen Daten.

## Grenzen und Nachweise

Der native Transport begrenzt Antworten standardmäßig auf 128 MiB und Anfragen auf 60 Sekunden. Die aktuelle Byte-Vektor-Schnittstelle ist noch kein durchgehender Streaming-Adapter für große Produktionsdateien. Der Server kann größere Dateien unterstützen; der Host muss die Client-Grenze berücksichtigen. Abbruchprüfungen erfolgen zwischen Upload-Schritten und beim Lesen von Antwortblöcken; ein bereits abgeschickter Commit kann nicht zurückgenommen werden.

SDK und nativer Adapter deklarieren Rust 1.90; egui 0.36 benötigt Rust 1.95. WASM betrifft ausschließlich `craftcloud-client`. Unit-, Vertrags- und Zustandstests werden durch den realen Roundtrip, Lease- und Konfliktprüfungen sowie die Workspace-Gates ergänzt. Ausführungsbelege und fehlende Plattformprüfungen stehen im AC-04-Bericht. SDK-Verteilung und Einbau in die eigentlichen Apps bleiben Folgeentscheidungen.
