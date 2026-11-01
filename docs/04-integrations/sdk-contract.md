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

`discover` prüft die API-Hauptversion zuerst und meldet eine fremde Hauptversion als `IncompatibleServer`; Server ohne die später ergänzten Felder werden akzeptiert (fehlende Felder erhalten Standardwerte). `login`, `complete_second_factor`, `refresh`, `restore_refresh_token`, `refresh_token`, `clear_tokens` und `logout` verwalten die Geräte-Sitzung. `get<T>` und `post<T, B>` verwenden denselben Authentifizierungspfad für die Organisations-, Workspace-, Projekt-, Datei- und Freigabe-DTOs. Listen werden mit `next_cursor` weitergeblättert.

`create_upload`, `upload_content` und `commit_version` machen die Upload-Schritte einzeln nutzbar. `PendingUpload` erhält die Upload-ID und den Idempotency-Key für einen weiteren Commit nach 412. `upload` fasst den gewöhnlichen Ablauf zusammen; `upload_with_cancel` nimmt ein geteiltes Abbruchsignal entgegen. Initiale Versionen verwenden `If-None-Match: *`, spätere Versionen `If-Match: "version:<basis>"`.

`download(file_id, Some(version_id))` lädt die unveränderliche Version und prüft `Repr-Digest` vor Rückgabe der Bytes. Nach dem Lesen von Datei-Metadaten muss der Host deren konkrete Kopfversion übergeben, damit zwischenzeitliche Commits nicht zu einer falschen lokalen Basis führen. `acquire_lock` verlängert zugleich die Lease; `release_lock` gibt sie frei. FigCraft-Dateien benötigen keine Lease.

## Lokale Dateien

`LinksStore::new(app_data_dir)` verwaltet `craftcloud-links.json`: lokaler Pfad, Server, Datei-ID, Basisversion und SHA-256. Zugangsdaten gehören nicht in diese Datei. Lokales Speichern geschieht zuerst. Ohne passende Verknüpfung bleibt es lokal; ein Cloud-Fehler erhält die lokalen Bytes und die bekannte Basis. Ein erneuter Upload ist eine ausdrückliche Aktion, keine Hintergrund-Warteschlange.

`VersionCache` erhält einen Stammordner namens `CraftCloud`; darunter folgen Server-Hash, Datei-ID und Versionsdatei. Pfadsegmente werden geprüft. Bereits vorhandene abweichende Bytes dürfen nicht überschrieben werden, weil die Gast-App die Cache-Datei inzwischen bearbeitet haben kann.

`FileLeaseHeartbeat` bleibt auf dem Worker aktiv, solange die App die Datei als offen meldet. Er erneuert alle 60 Sekunden und gibt beim Schließen frei. Die Freigabe führt der Heartbeat-Thread selbst nach seinem aktuellen Aufruf aus, sodass sie keinen laufenden Heartbeat überholt. Das Schließen wartet höchstens drei Sekunden darauf; hängt ein Aufruf länger, läuft er abgelöst weiter und die Lease verfällt andernfalls von selbst. `FileLeaseHeartbeat::start_keyed` und `wait_for_release(key, timeout)` erlauben dem Host, vor dem erneuten Öffnen derselben Datei die ausstehende Freigabe abzuwarten. Der Host muss Heartbeat-Fehler als Verlust der Bearbeitungserlaubnis behandeln. Der Server prüft weiterhin jede Schreiboperation.

Bei 423 stehen Abbruch und schreibgeschütztes Öffnen zur Wahl. Bei 412 bleibt die lokale Datei erhalten. „Beide behalten“ erstellt eine neue Datei mit Person und Datum im Namen. „Überschreiben“ benötigt eine erneute Bestätigung und einen Commit gegen die aktuell gelesene Kopfversion; ein weiterer Konflikt wird erneut angezeigt.

## Authentifizierung und TLS

Zugriffstokens bleiben im Speicher. `KeychainTokenStore` trennt Erneuerungstokens nach Server, Konto und App. Der Host speichert den nach jeder Rotation erhaltenen Wert und entfernt den Eintrag bei Abmeldung oder unbrauchbarer Sitzung. Teilen mehrere Clients oder Prozesse einen Eintrag (derselbe Server, dasselbe Konto, dieselbe App), liest der Client vor jeder Erneuerung den gespeicherten Wert (`RefreshTokenStore::load`) und übernimmt ihn, wenn ein anderer Halter bereits rotiert hat; Lesen, Rotieren und Speichern laufen unter einer Sperre (`RefreshTokenStore::lock_exclusive`). `BoundKeychainTokenStore::with_lock_dir` legt dafür eine Sperrdatei im App-Datenordner an und serialisiert dadurch auch mehrere Prozesse. Ohne Sperrverzeichnis werden nur Clients im selben Prozess serialisiert. Hält ein anderer Prozess die Sperrdatei länger als 10 s oder ist das Verzeichnis unbrauchbar, fällt der Client auf die Prozesssperre zurück, statt die Anmeldung zu blockieren. Die Demo legt ihre Daten ausschließlich in einem Benutzerverzeichnis ab (kein Rückfall auf ein gemeinsames Temp-Verzeichnis). Andernfalls würde ein zweiter Halter ein bereits rotiertes Token vorlegen und der Server die ganze Sitzungsfamilie widerrufen. Ohne Schlüsselbund gibt es keine Persistenz; Klartextdateien sind kein Fallback. Credentialtragende Debug-Ausgaben sind maskiert.

401 führt höchstens zu einer kontrollierten Erneuerung und Wiederholung der ursprünglichen Anfrage. Eine verlorene Refresh-Antwort wird nicht wiederholt, weil der Server ohne Gnadenfrist rotiert. Ein Netzwerkfehler, der beweisbar vor dem Senden der Anfrage auftrat (Namensauflösung, abgelehnte oder nicht zustande gekommene Verbindung: `CloudError::Unreachable`), lässt die Sitzung bei einer fehlgeschlagenen Erneuerung bestehen, weil der Server das Token nicht rotiert haben kann. Jeder Fehler mit unbekanntem Ausgang (`Offline`) und jede Ablehnung durch den Server beendet die Sitzung wie bisher.

`logout` erneuert bei 401 einmal das abgelaufene Zugriffstoken und wiederholt die Abmeldung, damit Geräte-Sitzung und Dateisperren auf dem Server enden. Gelingt das nicht, wird der lokale Zustand trotzdem gelöscht und der Fehler zurückgegeben; der Host zeigt dann „lokal abgemeldet, Server-Sitzung konnte nicht beendet werden“. Lehnt der Server auch das Erneuerungstoken ab, ist die Sitzung dort schon beendet und die Abmeldung gilt als erfolgreich.

### Zweiter Faktor

Hat das Konto TOTP aktiviert, antwortet `login` mit `CloudError::SecondFactorChallenge(challenge_id)` (401 `SECOND_FACTOR_REQUIRED`, Challenge fünf Minuten gültig, höchstens fünf Versuche, an die Client-Adresse gebunden). `complete_second_factor(challenge_id, Proof::totp(code))` beziehungsweise `Proof::recovery(code)` ruft `POST /api/v1/auth/device/login/second-factor` auf und hält danach die Sitzung wie nach `login`. Die Nachweise liegen in `Secret` (Speicher wird beim Freigeben überschrieben, `Debug` zeigt `***`). Ein falscher oder verbrauchter Nachweis ergibt `Unauthorized`; der Server zählt außerdem fortlaufende Fehlversuche je Konto und sperrt es nach 100 bis zum Zurücksetzen durch einen Operator, ohne dass die Antwort dies verrät. `craftcloud-egui` klassifiziert die Eingabe (`classify_second_factor`: sechs Ziffern oder zehn Zeichen Wiederherstellungscode) und sendet ungültige Formen gar nicht erst.

HTTPS ist verpflichtend, außer HTTP auf Loopback. Benutzerinformationen in der URL sind verboten. ureq verwendet rustls mit `RootCerts::PlatformVerifier` und verfolgt keine Weiterleitungen. Die Zertifikatsprüfung ist nicht abschaltbar; private Unternehmens-CAs werden über den Plattform-Zertifikatsspeicher eingerichtet.

## Widgets und Beispiele

`PickerState::show` liefert `Action`-Werte. `Translator` bindet das i18n-System der Gast-App ein; `DefaultTranslator(Language::English/German)` liefert Vorgaben. Gestaltung und Farben stammen aus dem egui-Kontext. Konflikt, Überschreibbestätigung, Sperre, Fortschritt und Offline-Hinweis sind eigene Zustände. Die Demo verarbeitet Aktionen auf einem Worker und verwirft überholte Ereignisse.

`crates/craftcloud-client-native/examples/roundtrip.rs` verwendet `CC_SERVER_URL`, `CC_EMAIL`, `CC_PASSWORD`, `CC_PROJECT_ID` sowie einen lokalen Dateipfad als Argument. Zugangsdaten werden nicht ausgegeben. Die Demo verwendet `CRAFTCLOUD_DEMO_SERVER`; synthetische Abnahmedaten können über `CRAFTCLOUD_DEMO_EMAIL` und `CRAFTCLOUD_DEMO_PASSWORD` übergeben werden. `CRAFTCLOUD_DEMO_DATA_DIR` isoliert ihre lokalen Daten.

## Grenzen und Nachweise

Der native Transport begrenzt Antworten standardmäßig auf 128 MiB. Zeitgrenzen: Namensauflösung und Verbindungsaufbau je 15 s, Senden des Anfragekopfs 15 s. Alle Anfragen außer Inhalts-Transfers (`.../content`: Login, Refresh, Listen, Commits) haben eine kurze Gesamtfrist von 60 s (Antwortkopf 30 s); ein Token-Refresh hält die Refresh-Sperren daher höchstens etwa eine Minute. Inhalts-Transfers haben keine feste Gesamtfrist: der Upload erhält ein mit seiner Größe wachsendes Budget (30 s plus Größe bei mindestens 32 KiB/s), der Download ein Budget aus dem `Content-Length` der Antwort (Längen über der Client-Grenze werden vor dem Lesen abgelehnt; ohne Länge gilt nur für Downloads das volle Budget, sonst 30 s), sodass 100 MiB auch über einen langsamen Uplink gelingen. Der Antwortkörper wird auf einem Hilfsthread gelesen, damit ein stehender Transfer den Aufrufer nach Ablauf des Budgets freigibt. Ureq 3.4 bietet keine Leerlauf-Frist je Lesevorgang; ein zum Stillstand gekommener Transfer endet daher erst mit diesem Budget oder durch Abbruch zwischen den Schritten. Die aktuelle Byte-Vektor-Schnittstelle ist noch kein durchgehender Streaming-Adapter für große Produktionsdateien. Der Server kann größere Dateien unterstützen; der Host muss die Client-Grenze berücksichtigen. Abbruchprüfungen erfolgen zwischen Upload-Schritten und beim Lesen von Antwortblöcken; ein bereits abgeschickter Commit kann nicht zurückgenommen werden.

SDK und nativer Adapter deklarieren Rust 1.90; egui 0.37 benötigt Rust 1.95 (Stand 2026-10-30 gleiche Linie wie PhotoCraft, FigCraft und craft-launcher: egui/eframe 0.37.0, ureq 3.5.0, rfd 0.17.2). WASM betrifft ausschließlich `craftcloud-client`. Unit-, Vertrags- und Zustandstests werden durch den realen Roundtrip, Lease- und Konfliktprüfungen sowie die Workspace-Gates ergänzt. Ausführungsbelege und fehlende Plattformprüfungen stehen im AC-04-Bericht. SDK-Verteilung und Einbau in die eigentlichen Apps bleiben Folgeentscheidungen.


## AC-10: Unternehmensanmeldung im Systembrowser

`CloudClient::begin_oidc` und `exchange_oidc` ergänzen die bisherige Geräte-Anmeldung. Der native Ablauf in `craftcloud-client-native::oidc` verwendet einen systemeigenen Browser und einen begrenzten Loopback-Listener. Provider-Redirect und native Rückleitung sind getrennt: Der Provider leitet ausschließlich zur fest konfigurierten Server-Callback-URI; der native Client erhält danach einen einmaligen, mit eigenem PKCE geschützten Abschlusscode. Er akzeptiert ausschließlich den erwarteten Host, Pfad und state. Ablauf, Begrenzungen und sichere Kontobindung stehen in der [Betriebsanleitung](../06-operations/sso-and-scim.md). Fehler lassen vorhandene lokale Dokumente und Dateifunktionen unberührt. Der normale AC-01-Gerätevertrag einschließlich Benutzerobjekt, rotierenden Tokens und Schlüsselbundablage bleibt gültig.
