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

## Retention, garbage collection and reconciliation abuse cases (PR-012, CC-F10)

| Abuse case | Mitigation | Test |
|---|---|---|
| Purge of live data, or before the retention has passed | only rows with `deleted_at` older than the retention, re-checked under the organization lock; a restored or live item is skipped | `a_trashed_file_is_purged_only_after_the_retention`, `a_trashed_project_is_purged_with_all_its_files`, `purging_a_project_closes_open_uploads_and_releases_their_reservation` |
| Version retention deletes the head or a version a restore still depends on | the head is never selected; references of remaining versions are cleared, the blob stays while any version references it | `version_retention_keeps_the_head_and_recent_versions` |
| Garbage collection deletes a blob that a version or an open upload still references | candidate scan is only a hint; the claim re-checks under the organization lock and a blob row lock (`SKIP LOCKED`); `staged` blobs and blobs of open sessions are never candidates | `blob_gc_deletes_unreferenced_blobs_only`, `gc_claims_never_take_referenced_or_young_blobs_and_never_go_negative` |
| Restore or commit races with garbage collection (dangling version) | both run under the organization lock; restore reads the blob `FOR SHARE`, commit locks the blob and requires `staged`, restore requires `committed`; a `deleting` blob is refused | `concurrent_restores_and_a_run_never_leave_a_dangling_version`, `commit_and_restore_never_reference_a_deleting_blob` |
| Crash between claiming a blob and deleting its object | the claim commits first and leaves `deleting`; the next run resumes it, counters were decremented exactly once and never go below zero | `a_crash_between_the_gc_phases_is_completed_by_the_next_run` |
| Quota counters drift after crashes or bugs | per organization recomputed from the rows under the lock; warning with organization id and deltas | `usage_drift_is_corrected_and_logged_with_ids_and_deltas` |
| Deleting a fresh object that is about to be recorded (crash window after publishing) | strays and temporary files are only removed after upload TTL + 1 hour (file system modification time) | `reconciliation_removes_old_strays_and_keeps_young_ones` |
| Bit rot or tampering of stored objects goes unnoticed | periodic SHA-256 re-hash with a byte budget; mismatch marks `corrupt`, a matching `corrupt` blob becomes `committed`; a missing object is reported, never deleted | `verification_marks_tampered_blobs_corrupt_and_clears_after_repair` |
| Two maintenance runs at once, or a runaway run | PostgreSQL advisory lock across processes; bounded batches and a time budget; stops between items on shutdown | `the_advisory_lock_admits_one_run_at_a_time`, `a_stopped_or_out_of_time_run_does_no_work`, `the_loop_runs_and_stops_on_shutdown` |
| A dry run or a broken step changes data | dry run counts only; a failing step is reported and the others still run | `a_dry_run_changes_nothing`, `a_failing_step_is_reported_and_the_others_still_run` |
| Others reading the maintenance status, or tenant data in it | operator-only action `MaintenanceRead`; the report holds counts and error codes only | `the_maintenance_status_is_for_the_operator_only` |
| Wrong, fresh or older database against the real store directory wipes the store (reconciliation sees every object as unknown) | database and store are bound by an identity in `storage_binding` and the marker file `craftcloud-store.json`; any disagreement disables storage use (503), garbage collection, reconciliation and verification (`STORE_BINDING_MISMATCH`); no automatic repair, deliberate `admin storage-bind --force` | `a_store_that_does_not_belong_to_the_database_is_never_touched`, `the_first_run_binds_database_and_store_and_keeps_the_binding`, `maintenance_once_dry_run_changes_nothing_and_a_real_run_purges` (CLI), `the_marker_is_written_once_atomically_and_read_only` |
| A bug or a restore error makes reconciliation destroy stray-looking objects | strays are only moved to `quarantine/<day>/` and kept `CRAFTCLOUD_QUARANTINE_DAYS`; a brake aborts the step when at least 100 and more than 10 % of the objects older than the grace period are unknown (`RECONCILE_BRAKE`) | `strays_are_quarantined_and_purged_after_the_quarantine_days`, `the_brake_stops_a_reconciliation_that_would_move_most_objects`, `quarantine_moves_objects_out_of_service_and_purges_old_days` |
| An older database backup is restored next to a newer store: the binding still matches and the job acts on stale truth (re-expires, purges, moves objects that the old database does not know) | binding epoch in the database and in the marker, raised by every destructive run (database first, marker second); marker ahead of the database (`DATABASE_OLDER_THAN_STORE`) or database more than one ahead (`MARKER_OLDER_THAN_DATABASE`) blocks all destructive steps while serving continues; one ahead is the crash window and is repaired; the operator accepts with `storage-bind --force`; `CRAFTCLOUD_MAINTENANCE_ENABLED=false` and the restore runbook | `a_restored_older_database_blocks_every_destructive_step_until_accepted`, `a_marker_that_is_older_than_the_database_blocks_destructive_steps`, `a_crash_between_the_database_and_the_marker_write_is_repaired`, `every_destructive_run_raises_the_epoch_in_the_database_and_the_marker`, `the_marker_epoch_round_trips_and_replacing_keeps_it_read_only` |
| A small store (few objects) is wiped because the wrong database knows none of them | the brake also applies at more than 50 % unknown objects (at least 10); every quarantine move logs a warning with the count | `a_small_store_with_a_wrong_database_hits_the_brake` |
| A wrong application clock makes retention, expiry or garbage collection fire early | the application clock is compared with the database `now()` at the start of a run; above 5 minutes all destructive steps are skipped (`CLOCK_DRIFT`) | `a_clock_that_differs_from_the_database_skips_the_destructive_steps` |
| One poisoned row blocks a whole step forever | every item runs in its own transaction; a failure is logged, counted (`item_errors`) and the step continues; the purge also moves a session that a version in another file references instead of deleting it | `a_failing_item_does_not_block_the_other_items`, `poisoned_items_at_the_head_do_not_starve_the_batch`, `a_purge_survives_an_upload_committed_into_another_file` |
| Residual risk | no lease for running downloads: on Windows the object cannot be deleted while a download holds it open (the blob stays `deleting` and is retried), on unix the download keeps reading the unlinked file; purged data stays in backups until they expire; a deleted-then-restored database without its object directory shows `missing_objects` until repaired; `storage-bind --force` is a deliberate override (the operator must verify the pairing first), an attacker with host and database access can rebind anyway; a failing item is retried only after 24 hours (the memory of failed items is per process: after a restart it is tried again at once); the status route and logs reveal organization ids only in the drift warning | see docs/gaps.md |

## Version and download abuse cases (PR-011, CC-F08)

| Abuse case | Mitigation | Test |
|---|---|---|
| Lost update by two writers on one base | compare-and-set on the head under the file row lock, 412 with the current head, never an overwrite; the loser's upload is kept | `two_writers_on_one_base_yield_one_version_and_one_412_and_the_loser_can_save_a_copy`, `parallel_commits_on_the_same_base_have_exactly_one_winner` |
| Revoked or demoted editor commits a finished upload | creator-only commit with fresh roles in the locking transaction; refusal leaves the upload stored and abortable | `revoked_editors_and_deleted_files_cannot_commit_and_keep_a_releasable_upload` |
| Commit of someone else's upload, or across workspaces and tenants | policy on the upload's creator, same-workspace check, uniform 404 | `a_retried_commit_returns_the_same_version_and_counts_bytes_once`, `versions_of_another_organization_are_invisible_and_cannot_be_committed_into`, `only_stored_unexpired_uploads_of_the_same_workspace_can_be_committed` |
| Double counting of bytes by retries | the upload id is the idempotency token; state transition and counters change in one transaction | `a_retried_commit_returns_the_same_version_and_counts_bytes_once`, `quota_counters_follow_commit_restore_and_recompute` |
| Content sniffing or script execution from downloaded files | always `application/octet-stream`, `nosniff`, `Content-Security-Policy: sandbox`, attachment disposition | `downloads_carry_the_safe_headers_and_the_exact_bytes` |
| Header injection through file names | the display name is reduced to a safe ASCII fallback plus percent-encoded `filename*`; no control characters or quotes survive | same, unit test `content_disposition_is_safe_for_hostile_names` |
| Silent delivery of corrupted data | SHA-256 while streaming, last chunk held back, body ends with an error, blob marked, later requests refuse | `a_corrupted_object_aborts_the_download_and_marks_the_blob` |
| Download after access was revoked or the file deleted | roles read fresh per request; trash answers 409 | `downloads_carry_the_safe_headers_and_the_exact_bytes` |
| Residual risk | range reads are not verified by the server (clients verify full downloads with `Repr-Digest`); a corrupt blob is only found by a full download until the verification job (CC-P02-07) exists; no cap on concurrent downloads | see docs/gaps.md |

Download hardening (PR-011 review): a client that stops reading is dropped after the idle timeout and the file handle is released, and a client below 16 KiB/s on average after 5 minutes is cut (`a_download_client_that_stops_reading_is_dropped_and_releases_the_file`, `a_download_below_the_minimum_rate_is_cancelled_after_the_grace_period`). `If-Range` is honored only for the exact strong ETag (`if_range_honors_a_range_only_for_the_exact_current_etag`). A missing object is reported as a storage outage and does not mark the blob (`a_missing_object_is_a_storage_outage_not_corruption`). `If-Match: *`, lists and weak tags are refused (`star_lists_and_weak_tags_are_not_preconditions`), and the commit itself rejects an upload from another workspace (`the_commit_itself_refuses_an_upload_from_another_workspace`).
