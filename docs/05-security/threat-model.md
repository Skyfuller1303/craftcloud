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
2. User A manipulates `workspace_id` of User B and obtains same-name file. *Mitigation (PR-006, organizations level):* repository scopes only come from policy permits, non-members get an identical 404; tests `a_member_of_one_organization_sees_nothing_of_another` (all org routes, unknown-id comparison, operator, swapped ids), `plain_members_cannot_write_and_non_members_do_not_exist_for_them`, `admins_manage_members_and_admins_but_never_touch_owners`, `concurrent_demotions_and_departures_never_leave_an_organization_without_an_owner`, `every_unsafe_route_needs_a_session_the_csrf_header_and_the_origin`.
3. A Viewer starts upload then role revoked before commit.
4. A stale signed URL continues working after explicit revoke.
5. Storage key constructed from `../../etc/passwd`.
6. Upload reservation bypasses quota with 10 parallel sessions.
7. Raw `Authorization` logged via tracing middleware.
8. Browser CSRF deletes a project. *Mitigation (PR-005):* every unsafe session request needs the CSRF header and a same-origin `Origin`; tests `csrf_header_and_origin_are_both_required_for_unsafe_methods`, `login_rejects_foreign_origins_and_cross_site_fetches`. Further session/auth tests: `invalid_sessions_are_401`, `idle_timeout_expires_sessions_and_activity_extends_them`, `absolute_timeout_ends_sessions_despite_activity`, `logout_revokes_the_session_and_logout_all_revokes_every_session_of_the_user`, `password_change_requires_the_current_password_and_revokes_other_sessions`, `password_reset_link_is_operator_only_single_use_and_expires`, `every_login_failure_looks_identical`, `account_rate_limit_is_per_email_and_returns_retry_after`, `secrets_never_reach_the_logs`, `only_failed_logins_count_and_block_per_account_and_address`, `flooding_with_fresh_addresses_cannot_evict_an_active_account_counter`, `ipv6_addresses_in_one_64_prefix_share_limits`, `verified_login_insert_waits_for_a_password_change_in_flight_and_then_refuses`, `issuing_a_link_revokes_the_sessions_of_that_account_immediately`.
9. Reverse proxy trusts arbitrary `X-Forwarded-Host` to send invite tokens to attacker domain. *Mitigation (PR-005/PR-007):* invitation and reset links are built only from the configured `CRAFTCLOUD_PUBLIC_URL`, never from `Host` or `X-Forwarded-*`; without a configured URL the route answers 503 `AUTH_UNAVAILABLE`. Tests: `who_may_invite_what` (link prefix `https://cloud.test/invite#token=`).
10. Backup/restore replaces secrets or tenant pointers with inconsistent versions.

## Security gate

No public beta before threat model reviewed by security owner, negative policy suite run, session handling independently reviewed, backup restore demonstrated, admin exposure restricted, transport encrypted, and documented incident reporting. `SECURITY.md` is a draft until official maintainers supply contacts.

## Invitation abuse cases (PR-007, CC-F03)

| Abuse case | Mitigation | Test |
|---|---|---|
| Token guessing or enumeration | 32 random bytes, only the SHA-256 is stored, uniform 400 `INVALID_INVITATION` for unknown, malformed, expired, used and revoked tokens, rate limit per client address | `accepting_creates_the_account_membership_workspace_role_and_a_session`, `accepting_is_rate_limited_per_address_and_invitation_creation_per_user` |
| Replay or double acceptance, also in parallel | single consume `UPDATE ... WHERE accepted_at IS NULL AND revoked_at IS NULL AND expires_at > now`, locked transaction; exactly one account and one membership | `concurrent_accepts_of_one_token_have_exactly_one_winner` |
| Link leaks through logs, referrers or proxies | token in the URL fragment (never sent to servers), accepted only in the request body, never logged; audit lines carry ids only | `tokens_and_addresses_never_reach_the_logs` |
| Stolen link used with another account | an existing account must be logged in as exactly the invited address (403 `INVITATION_EMAIL_MISMATCH`, anonymous 401 `LOGIN_REQUIRED`); link possession alone creates accounts only for addresses without account | `invitations_for_existing_accounts_need_that_account_logged_in` |
| Privilege escalation through invitations | nobody invites `owner`; workspace admins only `member`, own workspace, at most `editor`, only if enabled; the inviter's rights are re-evaluated at acceptance; a demoted inviter's links die | `who_may_invite_what`, `stale_invitations_of_demoted_or_disabled_inviters_and_changed_settings_are_refused` |
| Existing member upgrades their role through an invitation | organization role never changes, workspace role never lowered; org-only invitation for a member: 409 and stays open | `existing_members_keep_their_roles_and_workspace_roles_are_never_lowered` |
| Spam or mailbox flooding, invitation table growth | one open invitation per organization and address (409 `INVITATION_PENDING`, also under concurrency), 1000 open invitations per organization, creation rate limit per user, no e-mail is sent in this phase | `one_open_invitation_per_address_listing_revocation_and_expiry`, `concurrent_invitations_for_one_address_yield_exactly_one` |
| Cross-tenant invitation (workspace of another organization, IDOR) | composite foreign key `(org_id, workspace_id)`, workspace looked up inside the locked organization, 404 otherwise | `who_may_invite_what`, `workspaces_never_cross_organizations_and_swapped_ids_find_nothing` |
| Login CSRF through the anonymous accept route | Origin check as for login; with a cookie also the CSRF header | `every_unsafe_workspace_and_invitation_route_needs_session_csrf_and_origin`, `accepting_creates_the_account_membership_workspace_role_and_a_session` |
| Open (unverified) address claim | link possession is the proof until SMTP verification exists (ADR-0013); documented gap, links are short-lived (72 h), single use and revocable | see docs/gaps.md |

### Account squatting through invitations (residual risk, PR-007)

Until e-mail verification exists, link possession is the proof of an address (ADR-0013). An inviter who accepts their own link creates an instance-wide account for an address they may not own. The real owner of the address who is invited later by another organization receives `LOGIN_REQUIRED` (the account exists) and cannot take it over. This is a documented residual risk, not a fixed one; the full fix is e-mail verification and delivery.

| Aspect | Handling | Test |
|---|---|---|
| Provenance | `users.email_verified_at` stays NULL for every account (also the setup operator) and `users.created_via_invitation_id` records the invitation; origin organization and inviter stay derivable | `invitation_created_accounts_record_provenance_and_the_operator_can_list_them` |
| Detection | `GET /admin/users` (operator only, filter `?unverified=true`) lists accounts with origin; audit line `account.created_via_invitation` with account, invitation, organization and inviter ids | same test |
| No information leak | the `LOGIN_REQUIRED` answer of another organization's invitation contains nothing about the existing account's origin | `invitations_for_existing_accounts_need_that_account_logged_in` |
| Remedy | operator disables the squatted account (`POST /admin/users/{id}/disable`; sessions revoked, logins refused) and agrees a replacement with the real owner; break-glass steps in the deployment guide | same test |
| Not before production | email verification is required before multi-tenant production use | docs/gaps.md |

## File name and tenant abuse (PR-008, CC-F06)

| Abuse case | Mitigation | Test |
|---|---|---|
| Path traversal or device names through file names (`../`, NUL, `CON.txt`) | the name is display metadata only and never part of a path (storage keys will be ids); strict validation: NFC, 1 to 255 bytes, no control, separator or reserved characters, no Windows device names, no trailing dot or space, no bidi or zero-width characters | `file_names_are_validated_normalized_and_unique_by_key`, unit tests in `crates/craftcloud-core/src/names.rs` |
| Lookalike or duplicate names (NFC vs NFD, case, `ß`, fullwidth letters, ligatures) | uniqueness over the NFKC and case-folded key among live rows (display name stays NFC as typed), enforced by a partial unique index over sha256 of the key; reserved device names include CON, PRN, AUX, NUL, CONIN$, CONOUT$, CLOCK$, COM0-9, LPT0-9 | same, `concurrent_creations_of_one_name_yield_exactly_one_file` |
| Guessing ids of another tenant's projects or files (IDOR), ids of the wrong kind | scope resolved from the row, composite foreign keys, policy before any data access, uniform 404 | `projects_and_files_never_cross_organizations_workspaces_or_kinds`, `composite_foreign_keys_and_unique_indexes_hold_below_the_api` |
| Viewer or editor exceeding their role | exact policy actions, roles loaded freshly in the locked transaction | `project_role_matrix`, `file_role_matrix_and_metadata_defaults` |
| Names as personal data in logs | audit lines carry ids only | `the_file_name_never_reaches_a_path_or_the_logs` |
| Resurrecting data after the retention period, or into a taken name | restore checks the clock and the name key; 410 / 409 | `trash_restore_and_retention_follow_the_clock` |

## Object storage abuse cases (PR-009, CC-F09)

| Abuse case | Mitigation | Test |
|---|---|---|
| Path traversal through keys or names | paths are built only from a server generated `ObjectKey` (32 lowercase hex characters) or a validated temporary id; file names never reach storage; hostile strings do not parse | `only_server_generated_keys_reach_paths`, key unit tests |
| Partial or corrupt object becomes visible | data goes to `tmp/<id>.part` through synchronous std file writes (no deferred write errors), is verified (length, SHA-256, file length on the handle after fsync) and published by one hard link; failed, aborted or dropped writers remove their file | `a_write_failing_at_byte_n_leaves_nothing_behind`, `fsync_and_rename_failures_publish_nothing`, `dropping_or_aborting_a_writer_removes_its_temporary_file`, `length_and_checksum_mismatches_discard_the_upload` |
| Oversized upload or memory exhaustion | caller supplied hard byte limit per write, 64 KiB buffers and chunks (a 1 GiB stream used 26 MiB peak job memory in the manual run) | `the_byte_limit_is_enforced_and_kills_the_writer`, `large_one_gibibyte_streams_with_bounded_buffers` |
| Disk full | typed `Full` error (ENOSPC / `ERROR_DISK_FULL`), temporary file removed, later 507 | fault injection `FaultKind::Full`, `disk_full_conditions_map_to_full` (real ENOSPC not exercised) |
| Symlink or junction swap inside the storage tree | root canonicalized, root/`objects`/`tmp`/shard directories must be plain directories, files opened with `O_NOFOLLOW` or `FILE_FLAG_OPEN_REPARSE_POINT` and required to be regular files | `links_in_the_tree_are_refused` (skipped on this Windows host without symlink privilege; unix `O_NOFOLLOW` path unverified) |
| Overwriting a published object | exclusive create for temporary files, publishing by `hard_link` (fails atomically with `AlreadyExists`, never overwrites), random keys, published objects read-only | `write_finalize_read_head_verify_delete` |
| Cross-tenant inference through deduplication | no deduplication; equal contents get distinct keys | `equal_contents_get_distinct_keys_and_nothing_is_deduplicated` |
| Error messages leaking server paths | `StorageError` Display carries no paths; details only in server logs | `other_errors_keep_their_class_and_display_has_no_paths` |
| Tampering with stored data | `verify` re-reads and compares the SHA-256; reads of truncated files fail | `verify_detects_changed_data_and_truncated_files_fail_to_read` |

Out of the threat model: an attacker with write access to the storage directory on the host (who could swap a directory for a link between a check and an open, or read the data directly). The checks against links in the tree defend against mistakes and hostile restores, not against a local root-level attacker; protect the directory with file system permissions. A capability-based file API (such as `cap-std`) is not used for now. A cancelled publish can leave an unreferenced object or a `.part` file; the reconciliation job (CC-P02-07) removes them after a grace period.

## Upload and quota abuse cases (PR-010, CC-F07, CC-F10)

| Abuse case | Mitigation | Test |
|---|---|---|
| Quota bypass with parallel sessions | reservation under the organization lock (organization row, then workspace row); counters recomputable | `parallel_reservations_can_never_exceed_the_quota`, `quotas_block_at_the_exact_boundary_and_lowering_never_deletes`, `usage_counters_can_be_recomputed_from_the_rows` |
| Upload larger than declared, or memory exhaustion | the declared size is a hard byte cap, bounded buffers, per-read idle timeout (60 s), maximum file size from configuration | `bad_content_fails_the_session_releases_the_reservation_and_stores_nothing`, `a_64_mib_upload_streams_over_http_with_bounded_buffers` |
| Corrupt or substituted content | SHA-256 and length verified before publishing | same, `a_verified_upload_leaves_a_staged_blob_and_keeps_its_reservation` |
| Revoked editor completes an open upload (viewer/editor race, CC-F05) | write rights are re-evaluated with fresh roles under the lock after the bytes arrived; the object is deleted again | `an_editor_demoted_during_the_upload_stores_nothing` |
| File or project deleted during the upload | checked again when the blob is recorded; object deleted | `a_file_or_project_deleted_during_the_upload_stores_nothing` |
| Slot exhaustion by open or abandoned sessions | caps (10 per user, 200 per organization), expiry (24 h), reservation released when the client disconnects | `open_upload_caps_apply_per_user_and_per_organization`, `sessions_expire_with_the_clock_and_release_their_reservation`, `a_client_that_disconnects_mid_upload_releases_everything` |
| Two writers into one session | one transition `created -> receiving` under the lock; no resume | `only_one_put_at_a_time_and_no_abort_while_receiving`, `parallel_puts_of_one_session_have_exactly_one_winner` |
| Disk full | typed error, 507, temporary file removed, no blob, reservation released | `a_full_disk_answers_507_and_leaves_no_blob_no_object_and_no_reservation` |
| Replayed create requests | `Idempotency-Key` with a request fingerprint | `idempotency_keys_replay_the_same_session_and_reject_other_requests` |
| Guessing upload ids of other tenants | scope from the session row, policy first, uniform 404 | `upload_ids_of_another_organization_find_nothing` |
| Other workspace members hijacking a session | only the creator sends content; managers may read and abort | `upload_role_matrix_for_all_four_routes` |
| Residual risk | a streamed body is exempt from the global request limits, so a reverse proxy or firewall should limit concurrent upload connections; a server crash, a forced shutdown after the grace period or a panic right after publishing leaves a `receiving` session, a temporary file or a published object without a `blobs` row until expiry and reconciliation (CC-P02-07 must sweep objects without a `blobs` row that are older than the upload TTL) | see docs/gaps.md |

Additional upload hardening (PR-010 review): a stream is bound to the session's expiry and to a minimum average rate (5 minutes grace, 16 KiB/s), so a slow client cannot keep a slot and a reservation longer than the session lives (`a_stream_that_outlives_the_session_is_cancelled_and_releases_everything`, `a_body_below_the_minimum_rate_is_cancelled_but_a_fast_one_is_not`). Verification, publishing and recording run in a detached task with a guard that deletes the object again, so a cancelled request cannot leave a published object outside the quota (`cancelling_the_request_after_the_object_is_published_leaves_nothing_unaccounted`). The per-user cap is serialized with a lock on the user row, and a cross-organization race on one `Idempotency-Key` is a 409 (`idempotency_keys_and_the_per_user_cap_hold_across_organizations`). Lock order of an upload creation: organization row, user row, workspace row.
