# E2E-Abnahmeszenarien (Given / When / Then)

## E2E-01: Fresh setup

**Given** clean DB and new data volume, **When** an operator installs and redeems bootstrap token, **Then** exactly one initial instance admin exists and setup routes cannot create another admin. **And** token does not appear in app logs after use.

## E2E-02: Org isolation

**Given** Alice in Org A and Bob in Org B, each with file UUIDs, **When** Alice calls get/download/change for Bob's objects, **Then** no bytes or distinguishing metadata leak, and Bob's file remains unchanged.

## E2E-03: File roundtrip

**Given** Editor with 500 MB file, **When** upload and version commit succeed, **Then** streamed download is byte-identical; server uses bounded RAM and produces checksum evidence.

## E2E-04: Parallel writes

**Given** two editors read base version V1, **When** both commit V2 candidates concurrently, **Then** one succeeds and one gets version conflict; no overwrite; losing local copy is forbidden.

## E2E-05: Crash consistency

**Given** upload is in finalize or DB head transaction, **When** server crashes and restarts, **Then** no ready file points to missing content; orphan garbage can be reclaimed safely after retention.

## E2E-06: Permission revoked mid-flight

**Given** Editor starts upload, **When** admin changes user to Viewer before commit, **Then** finalize/commit is rejected (depending security contract), no unauthorized published revision.

## E2E-07: Operator backup/restore

**Given** multiple users, projects, file versions, **When** consistent backup is restored on fresh instance, **Then** byte checksums and permissions match, and stale sessions/secrets handled as documented.

## E2E-08: Desktop local-first

**Given** local file with no CraftCloud server configured, **When** user opens and saves, **Then** normal app flow works without login. **Given** configured server becomes unreachable, **When** user saves locally, **Then** local copy remains intact, cloud shows pending/unavailable rather than success.

## E2E-09: Browser security

**Given** logged-in admin, **When** another site submits a mutating cross-origin request without CSRF token, **Then** it fails. **When** a Viewer manually calls an admin API, **Then** the server denies it regardless of hidden UI controls.

## E2E-10: Version compatibility

**Given** v1 server, client tries unsupported future feature, **When** capability discovery responds unsupported, **Then** client hides/blocks it safely without unrecognized write attempts.
