# Threat model (STRIDE / trust boundary review)

## Assets

Benutzeridentitäten, Session-/Device-Tokens, Org-/Workspace-Metadaten, private Entwurfsdateien, Dateiversionen und -Hashes, Einladungen, Audit-Historie, Storage-Buckets, Secrets, Backup-Archive und Restore-Kontrolle. Betreiber mit Root-/DB-/Storagezugriff kann **unverschlüsselte Dateien lesen**; E2EE ist nicht Bestandteil des MVP.

## Attackers

- Anonymer Internetnutzer: Login-/Setup-/Share-Link-Abuse.
- Angemeldeter Viewer: IDOR, Schreib-/Role-Eskalation, fremde Workspace-Listen.
- Eingeladener Externer: Scope-Lateral-Movement.
- Kompromittierter Desktop-Token: Replay/Duration/Scopes.
- Bösartiger File Uploader: Parser-/Zip-Bomb/Path-Traversal/Quota-DoS.
- Fehlkonfigurierter Reverse Proxy: Host-/X-Forwarded-/Cookie-Bypass.
- Storage-Misconfiguration/Operator error: ungeschützte Buckets, Backup-Verlust.

## STRIDE-Matrix

| Risiko | Beispiel | Gegenmaßnahmen | Test |
|---|---|---|---|
| Spoofing | fremdes Invite/Sessiontoken | secure RNG, Hash-at-rest, TTL, revocation, MFA later | Token-Replay/Expiry |
| Tampering | paralleler Commit überschreibt Version | immutables, checksums, CAS If-Match, transactions | two writers, crash injection |
| Repudiation | Admin bestreitet Freigabe | audit event with actor/resource/request ID | event present, secrets absent |
| Information disclosure | ORG A sieht File B über UUID | tenant-scoped queries, deny-by-default | full IDOR matrix |
| DoS | unbounded upload/JSON/DB connections | bounded streams, limits, quotas, per actor rate limit | fuzz/large bodies |
| Elevation | viewer sends admin REST call | policy service + server enforcement | forged role/payload |
| SSRF | server fetches arbitrary URL | no arbitrary URL fetch in MVP; future DNS/private address guards | redirect/private IP |
| Supply chain | malicious dependency/container | lockfile, audits, signed release policy, least privilege | SBOM scan |

## High-priority abuse cases

1. `/setup/complete` race creates two owners. *Mitigation (PR-004):* the setup transaction claims the singleton row `instance_setup` first, so concurrent attempts serialize on its primary key; tests `twenty_concurrent_requests_create_exactly_one_operator` (HTTP, 20 requests, exactly one 201) and `concurrent_setups_produce_exactly_one_winner` (DB), rollback in `failure_inside_the_transaction_leaves_nothing_behind` and `setup_failure_rolls_back_everything`; wrong token and token-free probing in `setup_flow_status_wrong_token_success_and_repeat` and `wrong_token_is_forbidden_and_never_logged`.
2. User A manipulates `workspace_id` of User B and obtains same-name file.
3. A Viewer starts upload then role revoked before commit.
4. A stale signed URL continues working after explicit revoke.
5. Storage key constructed from `../../etc/passwd`.
6. Upload reservation bypasses quota with 10 parallel sessions.
7. Raw `Authorization` logged via tracing middleware.
8. Browser CSRF deletes a project.
9. Reverse proxy trusts arbitrary `X-Forwarded-Host` to send invite tokens to attacker domain.
10. Backup/restore replaces secrets or tenant pointers with inconsistent versions.

## Security gate

No public beta before threat model reviewed by security owner, negative policy suite run, session handling independently reviewed, backup restore demonstrated, admin exposure restricted, transport encrypted, and documented incident reporting. `SECURITY.md` is a draft until official maintainers supply contacts.
