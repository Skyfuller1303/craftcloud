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

Codes: `UNAUTHENTICATED` (401), `FORBIDDEN` (403; public existence leaks may require indistinguishable 404), `NOT_FOUND` (404), `METHOD_NOT_ALLOWED` (405), `PAYLOAD_TOO_LARGE` (413; Request-Body über dem konfigurierten Limit), `TIMEOUT` (503; Handler überschreitet das Zeitlimit — 408 bleibt langsamen Clients vorbehalten), `VALIDATION_ERROR` (422/400 by documented convention), `VERSION_CONFLICT` (412 when an `If-Match` precondition fails, see ADR-0009; 409 only for idempotency-key reuse with a different payload), `QUOTA_EXCEEDED` (413/409 per design), `RATE_LIMITED` (429), `STORAGE_UNAVAILABLE` (503), `INTERNAL` (500), `UNSUPPORTED_MEDIA_TYPE` (415). Setup (CC-F01): `SETUP_UNAVAILABLE` (503; no database configured or reachable), `INVALID_BOOTSTRAP_TOKEN` (403; wrong token **or** no bootstrap secret configured, indistinguishable for callers and independent of setup state; the real reason is only logged), `SETUP_ALREADY_COMPLETED` (409; only for a valid token). There is deliberately no `SETUP_DISABLED` code on the wire. Authentication (CC-F02): `UNAUTHENTICATED` (401; no valid session), `INVALID_CREDENTIALS` (401; identical for unknown e-mail, wrong password, missing password and disabled account), `CSRF_FAILED` (403; CSRF header or Origin check failed), `INVALID_CURRENT_PASSWORD` (403), `ACCOUNT_DISABLED` (409; operator-only reset route), `INVALID_RESET_TOKEN` (400; identical for unknown, malformed, expired and used tokens), `RATE_LIMITED` (429 with `Retry-After`; counters for login failures are per account and address and per account, requests are limited per client address with IPv6 grouped by /64), `AUTH_UNAVAILABLE` (503; no database, or `CRAFTCLOUD_PUBLIC_URL` missing where a link must be built). Browser sessions use the cookie `__Host-craftcloud_session` (HttpOnly, Secure, SameSite=Lax); every unsafe request authenticated by it needs the `X-CSRF-Token` header **and** a same-origin `Origin` (or no `Origin` and a `Sec-Fetch-Site` other than `cross-site`). Own sessions can be listed (`GET /auth/sessions`) and revoked (`DELETE /auth/sessions/{id}`; other users' ids answer `NOT_FOUND`). `logout` and `logout-all` also clear the cookie when they answer 401 and the request presented a session cookie. A password change whose session was revoked (or whose password was changed or reset) while the request was processed answers 401 `UNAUTHENTICATED`. The path `/auth/password/change` follows the CC-F02 briefing; `/auth/logout-all` and the reset routes are additions. `VALIDATION_ERROR` has `details.fields` (a list of invalid field names, nested as `organization.slug`) only for semantic field validation; an unparsable body, unknown field or missing field (including a missing token) answers `VALIDATION_ERROR` **without** `details` before any token check and independent of server state. Request values are never echoed. A setup request that times out (`TIMEOUT`, 503) may nevertheless have been committed; clients re-check `GET /api/v1/setup/status` before retrying. Maintain stable machine codes independent of translated UI copy.

## Pagination

Cursor-based, bounded page limit (default 50, hard cap chosen in implementation), stable sort by `(created_at, id)` or name+id; cursor encodes scope and order and is signed. Tenant switches invalidate cursor. Never return arbitrary row counts from client input.

## Example commit

```http
POST /api/v1/files/{file_id}/versions
Cookie: <browser-session>   # desktop token scheme pending D13/ADR
X-CSRF-Token: <token>
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
