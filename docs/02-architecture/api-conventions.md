# HTTP-API, Fehlerverhalten und Kompatibilität

## Normen (proposed)

- Basis `/api/v1`, JSON UTF-8, UTC-Timestamps RFC 3339, UUID-IDs. Blob-Upload ist binary streaming statt Base64-in-JSON.
- REST-Bereiche: `/info`, `/auth/*`, `/organizations`, `/workspaces`, `/projects`, `/files`, `/uploads`, `/versions`, `/audit`.
- Browser: sichere cookie-basierte Sessions, CSRF-Token + Origin-Prüfung für state changes; SameSite explicit; HttpOnly, Secure unter HTTPS. Desktop: eigener OAuth Device Authorization Flow **erst nach eigener Security-Entscheidung**, bis dahin kein eingebauter Passwort-Dialog im Launcher.
- Token-Autorisierung nicht aus UI-Rollen ableiten; serverseitig prüfen und Tenant scopen. HTTP-Caches für private Daten verhindern.
- Write operations: `Idempotency-Key` für create/commit; `If-Match` gegen remote head für Updates; `ETag` und `X-Request-ID` in Responses; API-Version-Negotiation über `/info` plus passende Schema-Version.
- Upload-Größenlimits, Timeouts, Quota, Content-Type- und Dateinamenbegrenzung. Keine `X-Forwarded-*` Trust-Entscheidung ohne konfigurierte Proxy-Whitelist.

## Fehlerformat

```json
{
  "error": {
    "code": "VERSION_CONFLICT",
    "message": "Remote version changed since the file was opened.",
    "request_id": "01H...",
    "details": {"current_version_id": "uuid"}
  }
}
```

Codes: `UNAUTHENTICATED` (401), `FORBIDDEN` (403; public existence leaks may require indistinguishable 404), `NOT_FOUND` (404), `VALIDATION_ERROR` (422/400 by documented convention), `VERSION_CONFLICT` (409/412 depending precondition), `QUOTA_EXCEEDED` (413/409 per design), `RATE_LIMITED` (429), `STORAGE_UNAVAILABLE` (503), `INTERNAL` (500). Maintain stable machine codes independent of translated UI copy.

## Pagination

Cursor-based, bounded page limit (default 50, hard cap chosen in implementation), stable sort by `(created_at, id)` or name+id; cursor encodes scope and order and is signed. Tenant switches invalidate cursor. Never return arbitrary row counts from client input.

## Example commit

```http
POST /api/v1/files/{file_id}/versions
Authorization: Bearer <device-token>
Idempotency-Key: f8e...
If-Match: "version:UUID"
Content-Type: application/json

{"upload_id":"uuid","parent_version_id":"uuid","client_sha256":"hex"}
```

Version conflict returns 412 and a safe current head identifier. **Proposal only**, not existing ArtCraft API.

## Backward compatibility

- Document supported API versions and minimum client version; no silent wire-format changes.
- Feature discovery via `/info` rather than guessing server release numbers.
- New fields additive when possible; unknown fields tolerated where safe. Unknown enum strings produce controlled fallback.
- Security-related semantics (privileges, token scopes, data location) require explicit versioning and review.
- Snapshot a golden OpenAPI artifact and compare breaking diffs in CI.
