# ADR-0025: SMTP delivery and verified email ownership

Status: accepted for local AC-09 implementation (2026-10-31). Supersedes ADR-0017 when email delivery is enabled.

## Decision

An optional SMTP transport sends transactional mail through a durable PostgreSQL outbox. With no `CRAFTCLOUD_MAIL_TRANSPORT`, existing invitation and reset link-copy behavior remains unchanged. Configured SMTP requires STARTTLS or implicit TLS with certificate verification, the AC-02 encryption key, and a configured public URL. Relay authentication is optional; username and password-file configuration must appear together. SMTP errors and logs never contain recipients, credentials, message bodies, or link tokens.

The triggering transaction inserts the outbox row. Link capabilities remain hashed in their domain tables; the queue contains only an authenticated encrypted token using the AC-02 key, the outbox UUID as owner, and AAD `email`. Templates are repository text and HTML files with literal escaped substitution, English fallback, and no tracking. Security notices contain no action links.

The worker claims bounded batches using leases and retries transient failures with exponential backoff, at most eight attempts within 24 hours. Permanent failure or exhaustion is audited; successful, failed, and expired messages lose their ciphertext. SMTP and PostgreSQL cannot commit atomically: delivery is at least once, with a stable Message-ID, so a crash after SMTP acceptance can cause a duplicate. Exactly-once delivery is not claimed. Ephemeral verification capabilities and queued mail are excluded from backups so restore does not revive pending email changes or send stale messages.

With delivery enabled, inviters never receive bearer links. Invitations to new addresses create accounts only after mailbox link possession and password selection, marking the address verified. Existing unverified accounts are not silently migrated to verified status. They can request verification; operators can initiate verification for them. Existing unverified accounts must verify before accepting another organization's invitation. Invitation responses do not reveal whether the address already has an account.

Self-service email changes require the current password and, when configured, a current second factor. A single-use confirmation sent to the new address expires after 24 hours; the old address receives a notice without an action link. The account address changes only on confirmation, after current account/security state and uniqueness checks. Tokens appear only in URL fragments and request bodies, never persistent browser storage.

An email-change request rechecks the authenticated session under the account lock. Password-reset issuance, password changes, factor changes, and account disable/enable invalidate pending changes through a security version. Confirmation revokes sessions, their edit locks, outstanding reset tokens, and pending login challenges. Expired address reservations cannot prevent a different account from requesting that address. Legacy copyable invitations are not mailbox proof: with SMTP enabled they must first be resent with a rotated token.

Durable queue admission serializes recipient/template and actor limits: five messages per recipient and template per hour, thirty per triggering person per hour, and at least sixty seconds between resends. Security state changes and their notifications share a transaction. Operator status exposes configuration, last successful delivery, and failed count only; test mail always targets the authenticated operator's own address.

If a security-notice limit is exhausted, the associated security change or new login rolls back and returns a rate-limit error. This preserves the atomic action-and-notice guarantee but can delay further account changes until the window expires. Silently dropping required notices was rejected; operators must take this availability tradeoff into account.

The actor UUID in the queue is provenance, without a user foreign key: notification admission must not acquire another user's row lock while holding the global admission lock. Operator verification runs in bounded, cursor-based batches and can be continued after its rate-limit cooldown. New browser sessions also generate the new-login notice; session refreshes do not. Existing host-only password-reset recovery continues to print its explicitly protected recovery link because host access is the recovery authority even when mail is unavailable.

## Consequences and alternatives

Migration 0019 adds mail persistence and verification capability state. Existing accounts retain their verification status. SMTP secrets use files and deployment examples must explicitly permit outbound SMTP networking. A development-only test sink proves real SMTP delivery, TLS enforcement, and retry behavior; it is absent from production installation.

Direct synchronous SMTP was rejected because it cannot roll back with database changes and would couple request latency to the mail server. Plaintext queued tokens were rejected because database access would expose live capabilities. Automatically trusting legacy addresses was rejected because it would preserve account squatting. Graph, OAuth, newsletters, inbound mail, and application DKIM remain outside AC-09.

Implementation evidence, dependency versions/licenses, remaining limitations, and executed acceptance checks are recorded in the AC-09 report.
