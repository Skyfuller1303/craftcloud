# Client-SDK: Contract, Rückwärtskompatibilität und Interface

## Ziel

Ein `craftcloud-client` als dünner, typed, testbarer Dienst für Rust-basierte Craft-Apps. Keine Geschäftslogik über `figcraft`-, `wordcraft`- oder `gridcraft`-Dateiformate; diese bleiben bei den Apps. Kein Netzwerk im `core`-Crate.

## Vorläufige Rust-API (Konzept, kein ausführbarer Code)

```rust
// DESIGN PSEUDOCODE — signatures may change after RFC.
trait CloudTransport { /* request(), stream(), cancellation(), tls() */ }
struct CloudClient<T: CloudTransport> { /* endpoint, auth, transport */ }
impl<T: CloudTransport> CloudClient<T> {
    async fn capabilities(&self) -> Result<Capabilities, CloudError>;
    async fn list_workspaces(&self) -> Result<Vec<Workspace>, CloudError>;
    async fn upload_blob(&self, req: UploadRequest, bytes: ByteStream)
        -> Result<VerifiedUpload, CloudError>;
    async fn commit_version(&self, file: FileId, base: VersionId,
        verified: VerifiedUpload, key: IdempotencyKey)
        -> Result<CommittedVersion, CloudError>;
}
```

## Client-Fehlertypen

`Unauthorized`, `Forbidden`, `IncompatibleServer`, `Timeout`, `Offline`, `RateLimited{retry_after}`, `QuotaExceeded`, `ChecksumMismatch`, `VersionConflict{remote_head}`, `StorageUnavailable`, `UnknownServer`. Fehler werden nicht als Panics umgesetzt; UI erhält recoverable results und lokales Speichern bleibt möglich.

## Datenschutz und Trust

Discovery URL ist explizit, auf erlaubte Schemes/Origins validiert; kein dynamisches Cloud Auto-Discovery durch ungeprüfte öffentliche URL. Redirects verlieren sensitive Auth-Header bei Originwechsel. Eigene CA nur nach expliziter User-/Admin-Konfiguration; TLS verification nicht überspringen. Telemetrie standardmäßig aus.

## Testmatrix

Fake HTTP server mit 200/401/412/429/503, partial-body, delay, redirect, invalid JSON and massive body; Integration mit echtem Test-Backend; kompatible Spec-Versionen; Speicher-/Upload-Queue; offline first; cancellation. `craft-ai-integrations` bietet ein Beispiel für Trennung von wasm-sicherem Core und nativer IO-Schicht.
