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

Codes: `UNAUTHENTICATED` (401), `FORBIDDEN` (403; public existence leaks may require indistinguishable 404), `NOT_FOUND` (404), `METHOD_NOT_ALLOWED` (405), `PAYLOAD_TOO_LARGE` (413; Request-Body über dem konfigurierten Limit), `TIMEOUT` (503; Handler überschreitet das Zeitlimit — 408 bleibt langsamen Clients vorbehalten), `VALIDATION_ERROR` (422/400 by documented convention), `VERSION_CONFLICT` (412 when an `If-Match` precondition fails, see ADR-0009; 409 only for idempotency-key reuse with a different payload), `QUOTA_EXCEEDED` (413/409 per design), `RATE_LIMITED` (429), `STORAGE_UNAVAILABLE` (503), `INTERNAL` (500), `UNSUPPORTED_MEDIA_TYPE` (415). Setup (CC-F01): `SETUP_UNAVAILABLE` (503; no database configured or reachable), `INVALID_BOOTSTRAP_TOKEN` (403; wrong token **or** no bootstrap secret configured, indistinguishable for callers and independent of setup state; the real reason is only logged), `SETUP_ALREADY_COMPLETED` (409; only for a valid token). There is deliberately no `SETUP_DISABLED` code on the wire. Authentication (CC-F02): `UNAUTHENTICATED` (401; no valid session), `INVALID_CREDENTIALS` (401; identical for unknown e-mail, wrong password, missing password and disabled account), `CSRF_FAILED` (403; CSRF header or Origin check failed), `INVALID_CURRENT_PASSWORD` (403), `ACCOUNT_DISABLED` (409; operator-only reset route), `INVALID_RESET_TOKEN` (400; identical for unknown, malformed, expired and used tokens), `RATE_LIMITED` (429 with `Retry-After`; counters for login failures are per account and address and per account, requests are limited per client address with IPv6 grouped by /64), `AUTH_UNAVAILABLE` (503; no database, or `CRAFTCLOUD_PUBLIC_URL` missing where a link must be built). Browser sessions use the cookie `__Host-craftcloud_session` (HttpOnly, Secure, SameSite=Lax); every unsafe request authenticated by it needs the `X-CSRF-Token` header **and** a same-origin `Origin` (or no `Origin` and a `Sec-Fetch-Site` other than `cross-site`). Own sessions can be listed (`GET /auth/sessions`) and revoked (`DELETE /auth/sessions/{id}`; other users' ids answer `NOT_FOUND`). `logout` and `logout-all` also clear the cookie when they answer 401 and the request presented a session cookie. A password change whose session was revoked (or whose password was changed or reset) while the request was processed answers 401 `UNAUTHENTICATED`. The path `/auth/password/change` follows the CC-F02 briefing; `/auth/logout-all` and the reset routes are additions. `VALIDATION_ERROR` has `details.fields` (a list of invalid field names, nested as `organization.slug`) only for semantic field validation; an unparsable body, unknown field or missing field (including a missing token) answers `VALIDATION_ERROR` **without** `details` before any token check and independent of server state. Request values are never echoed. A setup request that times out (`TIMEOUT`, 503) may nevertheless have been committed; clients re-check `GET /api/v1/setup/status` before retrying. Organizations and RBAC (CC-F04/F05): `FORBIDDEN` (403) is only given to callers who may already see the resource (for example a plain member trying to change settings); callers who cannot see it (non-members, the instance operator without membership, unknown ids) get an identical `NOT_FOUND` (404). `ALREADY_MEMBER` (409), `LAST_OWNER` (409; an organization always keeps one owner), `CANNOT_DISABLE_OPERATOR` (409; disabling any other account is allowed even if it leaves organizations without an enabled owner, the 200 response lists them in `orphaned_organization_ids`), `PRECONDITION_REQUIRED` (428; `PATCH /organizations/{id}` needs `If-Match`), `VERSION_CONFLICT` (412; the ETag `"org-v<n>"` of the organization changed, the current ETag is in the response header), `INVALID_CURSOR` (400). Maintain stable machine codes independent of translated UI copy.

## Pagination

Cursor-based, bounded page limit (default 50, hard cap 100: larger values are clamped, `0` or non-numeric values answer 422), stable sort by `(created_at, id)` (members: join time and user id); cursors never contain names, e-mail addresses or other personal data; the opaque cursor encodes listing, scope (organization or user) and the last key, and answers 400 `INVALID_CURSOR` when used for another listing or scope. Implemented as `{"items": [...], "next_cursor": string|null}`. **Deviation:** the cursor is not yet signed; a tampered cursor can only change the keyset position inside a listing the caller may already read because every query filters by scope in SQL. Signing follows with a server secret. Tenant switches invalidate cursor. Never return arbitrary row counts from client input.

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
- Contract (implemented, CC-P01-09): `crates/craftcloud-api/tests/contract.rs` fails on drift between `contracts/openapi.yaml` and the served routes in both directions, checks that every error response uses the `Error` schema and that real errors carry `request_id` equal to the `x-request-id` header and `Cache-Control: no-store`. `contracts/openapi.snapshot` lists method, path, authentication, request/response schema and response codes per operation; after an intentional contract change run `cargo xtask openapi-snapshot --update` and commit the diff. `/health/ready` and `GET /admin/health` exist since PR-017 (CC-F15); `/metrics` is deliberately not part of the contract (it is served on a separate internal listener only).

## Workspaces und Einladungen (PR-007)

- Workspaces: `ETag` ist `"ws-v<version>"` (analog `"org-v<version>"`); `PATCH /workspaces/{id}` verlangt `If-Match` (428 `PRECONDITION_REQUIRED` ohne, 412 `VERSION_CONFLICT` bei veraltetem Wert, die Antwort trägt den aktuellen ETag). Die Organisation wird immer aus der Workspace-Zeile bestimmt, nie aus einem Client-Parameter; unbekannte, fremde und nicht sichtbare Workspaces antworten identisch mit 404.
- Neue Codes: `INVITATION_PENDING` (409; für diese Organisation und Adresse existiert eine offene Einladung), `INVITATION_LIMIT` (409; 1000 offene Einladungen je Organisation), `INVALID_INVITATION` (400; identisch für unbekannte, abgelaufene, benutzte, widerrufene, fehlerhafte und nicht mehr berechtigte Einladungen sowie deaktivierte Zielkonten), `LOGIN_REQUIRED` (401; die Adresse gehört zu einem bestehenden Konto, der Aufrufer ist nicht angemeldet), `INVITATION_EMAIL_MISMATCH` (403; angemeldet mit einem anderen Konto), `ALREADY_MEMBER` (409; auch bei Einladungen, die nur die Organisation betreffen, und bei Workspace-Mitgliedern).
- `POST /invitations/accept` ist anonym zulässig (Origin-Prüfung wie beim Login, Ratenbegrenzung je Client-Adresse); mit Sitzungscookie gilt zusätzlich der CSRF-Header. Das Token liegt im Fragment des Einladungslinks und wird nur im Request-Body gesendet, nie in einer URL oder einem Log.
- Listen: `GET /organizations/{id}/workspaces` (Erstellzeit, ID), `GET /workspaces/{id}/members` (Beitrittszeit, Nutzer-ID), `GET /organizations/{id}/invitations` (nur ID); Cursor enthalten wie bisher keine personenbezogenen Daten.
- Operator account listing: `GET /admin/users` (creation time, id; cursor bound to the `unverified` filter) returns instance-level account metadata including `email_verified` and the invitation `origin`. The reply of an accepted invitation or of `LOGIN_REQUIRED` never reveals who created an account or from which organization.

## Projekte und Dateimetadaten (PR-008)

- ETags: Projekte `"prj-v<version>"`, Dateien `"file-v<version>"`; `PATCH` verlangt `If-Match` (428 ohne, 412 `VERSION_CONFLICT` bei veraltetem oder fehlerhaftem Wert, die Antwort trägt den aktuellen ETag). Löschen und Wiederherstellen erhöhen die Version, verlangen aber kein `If-Match` und sind idempotent (`DELETE` auf Gelöschtes: 204 ohne Änderung, `restore` auf Lebendes: 200 ohne Änderung).
- Neue Codes: `PROJECT_NAME_TAKEN` / `FILE_NAME_TAKEN` (409; Name nach NFKC und Casefold schon vergeben, z. B. `Report.psd` und `Ｒeport.psd`; der angezeigte Name bleibt wie getippt in NFC, auch beim Wiederherstellen), `PROJECT_DELETED` (409; Schreibaktion auf ein Projekt im Papierkorb oder auf eine seiner Dateien), `FILE_DELETED` (409; Umbenennen einer Datei im Papierkorb), `RETENTION_EXPIRED` (410; Wiederherstellen nach Ablauf des Aufbewahrungsfensters; die Metadaten bleiben bis zur Löschung lesbar).
- Listen (`GET /workspaces/{id}/projects`, `GET /projects/{id}/files`): Schlüsselmenge nach (Erstellzeit, ID), `?deleted=true` zeigt den Papierkorb; der Cursor enthält nur Zeitstempel und ID und ist an Liste, Elternobjekt und den `deleted`-Schalter gebunden. Neue Einträge erscheinen immer hinter dem Cursor (stabil).
- Dateinamen sind untrusted Anzeigemetadaten und nie Teil eines Pfads; Audit-Zeilen und Logs enthalten nur IDs, keine Projekt- oder Dateinamen.

## Uploads und Quota (PR-010)

- Neue Codes: `QUOTA_EXCEEDED` (413; Quota der Organisation oder des Workspace würde überschritten, die Meldung nennt keine Zahlen anderer Workspaces), `UPLOAD_LIMIT` (409; zu viele offene Sitzungen), `IDEMPOTENCY_KEY_REUSED` (409; `Idempotency-Key` mit anderer Anfrage), `UPLOAD_IN_PROGRESS` (409; Inhalt wird gerade empfangen), `UPLOAD_CLOSED` (409; Sitzung beendet, abgebrochen, fehlgeschlagen oder gespeichert, nicht fortsetzbar), `UPLOAD_EXPIRED` (410), `LENGTH_MISMATCH` und `CHECKSUM_MISMATCH` (422), `INSUFFICIENT_STORAGE` (507; Gerät voll), `STORAGE_UNAVAILABLE` (503; kein Speicherverzeichnis konfiguriert), `REQUEST_TIMEOUT` (408; der Upload-Body stand 60 Sekunden still), `UPLOAD_INTERRUPTED` (400).
- `Idempotency-Key` (1 bis 128 sichtbare ASCII-Zeichen) gilt je Nutzer für `POST /files/{id}/uploads`: gleiche Anfrage (Datei, Größe, Hash) liefert dieselbe Sitzung (200 statt 201), andere Anfrage 409.
- `PUT /uploads/{id}/content` ist vom globalen Body-Limit und Handler-Zeitlimit ausgenommen; die Grenze ist die deklarierte Größe. Quota-Werte sind `null` (unbegrenzt) oder Byte-Zahlen; das Weglassen des Schlüssels ist ein 422, nie "unbegrenzt".
- Audit-Zeilen (`upload.create`, `upload.content`, `upload.abort`, `workspace.quota`, `organization.quota`) enthalten nur IDs, keine Dateinamen und keine Hashes.
- Upload-Daten kommen nicht langsamer als im Mittel 16 KiB/s an (nach 5 Minuten Schonfrist; sonst 408 `REQUEST_TIMEOUT`), und die Übertragung endet spätestens mit dem Ablauf der Sitzung (410 `UPLOAD_EXPIRED`). Mit dem Abschluss des Bodys laufen Prüfung, Veröffentlichung und Eintrag unabhängig von der Verbindung zu Ende.
- Quota: Das effektive Limit eines Workspaces ist das Minimum aus Workspace- und Organisations-Quota (`effective_limit_bytes` in der Nutzungsliste). Eine niedrigere Organisations-Quota ändert die Workspace-Quotas nicht.

## Versionen und Downloads (PR-011)

- Zwei Etags je Datei: Metadaten `"file-v<n>"` (nur für `PATCH /files/{id}`), Inhalt `"version:<uuid>"` (Kopfversion; für `If-Match` / `If-None-Match` beim Commit und Restore sowie in Download- und Versionsantworten). Ein Commit ändert beide.
- Commit (`POST /files/{id}/versions`): erste Version mit `If-None-Match: *`, danach `If-Match` auf den Kopf; keines davon 428, beide 422, abweichender Kopf 412 `VERSION_CONFLICT` mit `details.current_version_id` (null ohne Version) und `ETag`-Header (ADR-0009). Wiederholung desselben Uploads: 200 mit der vorhandenen Version. Ein mitgesendeter `Idempotency-Key` muss dem des Uploads entsprechen (409 `IDEMPOTENCY_KEY_REUSED`).
- Neue Codes: `UPLOAD_INCOMPLETE` (409; Upload noch nicht gespeichert), `BLOB_CORRUPT` (500; gespeicherte Daten bestehen die Prüfung nicht), `RANGE_NOT_SATISFIABLE` (416 mit `Content-Range: bytes */<Größe>`).
- Downloads: immer `application/octet-stream`, `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox`, `Cache-Control: no-store`, `Content-Disposition: attachment` mit bereinigtem ASCII-Namen und `filename*`, `Repr-Digest: sha-256=:<base64>:` (RFC 9530, Digest der ganzen Version, auch bei Range), `ETag`, `Accept-Ranges: bytes`. Ein Byte-Bereich wird mit 206 beantwortet; mehrere Bereiche, andere Einheiten und fehlerhafte Werte werden ignoriert (200 mit dem ganzen Inhalt). Bei einem Integritätsfehler endet der Body mit einem Fehler (Verbindungsabbruch), nie sauber.
- Versionslisten sind nach Nummer absteigend paginiert; der Cursor enthält nur die Nummer und ist an die Datei gebunden.
- `If-Match` bei Commit und Restore akzeptiert genau einen starken Tag `"version:<uuid>"`; `*`, Listen und schwache Tags (`W/`) sind 422 `VALIDATION_ERROR` (sie würden das Compare-and-Set aushebeln oder sind mehrdeutig). Ein anderer, gültig zitierter Tag ist ein gewöhnlicher Konflikt (412).
- `If-Range` bei Downloads: das Range wird nur beachtet, wenn der Wert exakt dem starken Etag `"version:<uuid>"` der ausgelieferten Version entspricht; schwache Tags, Datumsangaben und alles andere liefern den ganzen Inhalt mit 200.
- Download-Clients, die 60 Sekunden keinen Block abnehmen oder nach 5 Minuten im Mittel unter 16 KiB/s bleiben, werden getrennt (gleiche Regel wie beim Upload). Ein Objekt, das der Speicher gerade nicht liefert (fehlende Datei, nicht eingehängter Datenträger), ist 503 `STORAGE_UNAVAILABLE` und markiert den Blob nicht; nur ein nachgewiesener Hash- oder Längenfehler markiert ihn als `corrupt`.
