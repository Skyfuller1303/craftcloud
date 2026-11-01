# ADR-0026: Organisation-owned OIDC and SCIM

Status: implementation decision for AC-10, pending independent security and maintainer review.

## Decision

Each organization owns one OIDC provider. Only its active owners configure it. The instance operator can disable OIDC globally without affecting local recovery. Authorization Code with PKCE S256, state, nonce, verified discovery and JWKS is mandatory. The callback is derived exclusively from the configured public URL. Network destinations are HTTPS and public, with DNS results validated and pinned for each request, bounded responses, no redirects and short deadlines. Private-network providers require a separately reviewed operator-controlled network exception; an organization owner cannot reach internal services by supplying a discovery URL.

An external identity is bound to the exact issuer and subject, with global uniqueness of that pair. A known identity never follows an email change. An existing account can be linked only after its verified address matches the provider's verified address and its current local password has been proved; possession of an email claim alone never authenticates that account. A new account requires a verified allowed-domain address and enabled just-in-time creation, receives only the member role, and is checked against seat limits.

SSO provenance is stored separately for each browser or device session and organization. Local owner recovery and the instance operator remain exempt from SSO-only policy. The provider's MFA satisfies organization MFA only when the owner allows it and the signed amr contains mfa; otherwise CraftCloud TOTP is required when the organization requires MFA or the account has configured it. Desktop clients open the system browser, bind an ephemeral loopback listener, and redeem a single-use, short-lived completion code with their own PKCE verifier. Provider tokens never reach the client. The fixed server callback remains the provider redirect; only the completion code is sent to a validated literal loopback redirect.

SCIM uses a separate random per-organization token, revealed once and stored as SHA-256. Rotation invalidates the previous token. Users and groups follow the bounded RFC 7643/7644 subset specified by AC-10. Provisioning never silently attaches an existing account by email or changes an owner or operator. A managed member later promoted to organization administrator remains subject to deprovisioning; that status change preserves its role and never grants new privileges. Deactivation disables only the managed organization's membership, removes its leases, schedules live-link revocation, and invalidates all older sessions for that organization through a durable cutoff. Other organizations remain usable. Reactivation does not revive those old sessions.

Group mappings can grant only viewer or editor in a workspace of the same organization. Explicit manual workspace memberships take precedence over SCIM: SCIM creates and updates only memberships recorded as SCIM-owned, never overwrites a pre-existing manual membership, and relinquishes ownership when the actual role diverges from its last applied value. Multiple managed groups combine to their maximum role. Removal deletes only a still-owned unchanged membership. No SCIM input grants an organization administrator role.

Persistent provider configuration, identity bindings, SCIM resources and mapping ownership belong in backups. Bearer token hashes, login state, pending linking or MFA grants, completion codes, session provenance and organization session cutoffs are ephemeral security state and are excluded; restored instances require new logins and a new SCIM token. Rollback uses a pre-migration backup in an isolated instance; migration 0021 is forward-only.

## Alternatives

Email-only linking was rejected because addresses can be reassigned. Provider passwords in native apps and embedded browsers were rejected. Global account disabling during SCIM deprovisioning was rejected because accounts can belong to multiple tenants. Replacing manual roles from group payloads was rejected because it could erase independent grants. Arbitrary discovery fetches and redirects were rejected because they expose the internal network and credentials.

## Verification

Acceptance requires the real-container IdP abuse suite, SCIM isolation and revocation tests, desktop loopback tests, EN/DE accessible UI tests, and executed cargo xtask ci and cargo xtask db-test. The AC-10 report records actual results and any remaining deviation; this decision document is not a test result.


## Implementation refinements

A separate one-use launch capability is distinct from the provider state. The first actual browser to claim that launch receives a new binding cookie; provider state is not disclosed to the initiating native request. This prevents cross-browser login substitution. A native completion code expires after sixty seconds and is redeemed with its own PKCE verifier.

SCIM externalId can explicitly provide the stable OIDC subject for a newly provisioned passwordless account. The verified provider subject, tenant configuration and managed membership must all agree; email equality is never the binding authority. Directory userName and displayName changes remain tenant metadata rather than rewriting the global account mailbox. A bound externalId is immutable. Local verified accounts outside the organization can join only after current local-password proof and enabled JIT, with the same seat check as new accounts.

Provider-MFA recognition is evaluated against the current organization policy, and local factor assurance requires a still-enabled local factor. A passwordless account without recognized provider MFA is refused when organization MFA is required. Controlled operator password recovery enables later local TOTP onboarding; a dedicated passwordless enrollment protocol is deferred rather than weakening the existing password-based factor-management proof. SCIM removal keeps the global identity and requires controlled re-provisioning if a later POST collides with its retained mailbox.


Global issuer/subject identities can authenticate another explicitly configured organization only with an active membership, or after that organization's verified-domain JIT admission and seat check; the unique identity is never duplicated or chosen by email. Organization-specific session cutoffs return 403 ORGANIZATION_REAUTHENTICATION_REQUIRED for otherwise live credentials, including upload finalization, so SDKs and the browser do not erase valid access to other tenants as though the global credential had expired.

The email-domain hint selects an organization only when exactly one enabled configuration matches. Ambiguous domains do not select an arbitrary tenant; SDK/API callers can start a specific organization explicitly. A multiple-organization chooser in the browser is deferred.

Successful SCIM operations retain their existing transactional audit events. Failed requests with a currently valid organization SCIM bearer are recorded as bounded, aggregated operation failures without payloads or credentials. Invalid, foreign or revoked bearers do not create persistent anonymous audit floods. The failure-audit path is best effort, has a two-second absolute budget and cannot authorize a resource operation.

Account recovery is an independent global-account claim boundary. Consuming a password-reset grant detaches all external identities and user-bound pending OIDC completions in the same transaction as the password update and session revocation. A later company login must obtain fresh explicit consent with the recovered local password. Known issuer/subject resolution is rechecked under the user lock so a proof resolved before concurrent recovery cannot recreate the old credential. Preserving previously intentional links would require explicit consent provenance; unconditional detachment is the conservative decision for this release. This prevents a tenant-controlled JIT mailbox preclaim from surviving legitimate recovery and later membership in another organization.


## Review fixes

The independent review of the first AC-10 implementation led to the following decisions. Migration 0021 is not released yet and was changed in place.

**Domain ownership (D1).** An entry in an organization's allowed domains is only a claim. It becomes effective for just-in-time account creation, SCIM user creation (the domain of `userName`), the login hint and the use of an IdP email claim only after the instance operator approved it. Unapproved domains are stored and shown to the owner as pending, but they are inert. A domain can be approved for at most one organization (a unique index on the lower-case domain among approved claims; the API accepts only lower-case ASCII, with punycode for internationalized names). Approval and rejection are operator-only, available through the operator API and the System screen, and audited as `sso.domain_approved` and `sso.domain_rejected`. Removing a domain from the configuration removes its claim, and with it any approval. An account that is created by JIT receives a verified address only because the domain was approved for that organization. DNS TXT proof of control is a possible later option and is listed in `docs/gaps.md`.

**Provider MFA scope (D2).** The provider's `amr=mfa` counts only for the organization of the SSO binding. It never replaces a configured local TOTP factor: an account with a local factor must still present it, so the earlier refinement that let provider MFA stand in for a configured factor no longer applies. A locked-out local factor stays locked whatever the provider asserts. Another organization's `require_two_factor` is evaluated against the real local factor state, because the binding belongs to one organization only. An external identity is never linked to an account with an instance role.

**Policy tightening.** Turning `sso_required` on, or switching off trust in provider MFA, enqueues the same durable FigCraft link revocations as an organization-wide two-factor requirement.

**SCIM authentication and abuse limits.** The SCIM token hash is verified without any row lock first; only a valid token locks the organization and is checked again under the lock. Unauthenticated or throttled requests no longer open transactions or write audit rows. `/scim/v2/*` has its own per-address limit (`scim_ip_limit`, class `scim`). The routes OIDC `start`, `authorize` and `callback` share a dedicated per-address limit (`oidc_ip_limit`, class `oidc`). Pending login state is capped per organization and, generously, globally. Requests to identity providers are additionally limited per provider host, so one slow provider cannot occupy all concurrent provider requests.

**Operational details.** Pending sign-ins that concern a known account are tied to it (`user_id`), so recovery of that account deletes them. Session provenance rows are removed by foreign-key cascade when their browser or device session is deleted. When the operator disables OIDC for the instance, `sso_required` is treated as not required so members are not locked out. A native (desktop) sign-in is completed only after the person confirmed the device and client in the browser (`POST /auth/oidc/pending` describes it, `confirm_desktop` is mandatory for native flows). The SDK's `Debug` output redacts the authorization URL, which carries the one-use flow capability, and the PKCE and state values.
