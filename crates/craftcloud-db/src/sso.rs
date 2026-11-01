//! Current organization login policy and session provenance.
use crate::DbError;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

/// Result of checking a credential against one organization's current policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPolicy {
    /// The credential is usable; its second-factor assurance is explicit.
    Allowed { mfa: bool, sso: bool },
    /// This member must authenticate through this organization's provider.
    SsoRequired,
    /// SCIM invalidated credentials created before the organization's cutoff.
    Revoked,
}

/// Checks current state for a session; callers retain ordinary resource authorization.
pub async fn session_policy(
    pool: &PgPool,
    org_id: Uuid,
    user_id: Uuid,
    kind: &str,
    session_id: Uuid,
    created_at: OffsetDateTime,
) -> Result<SessionPolicy, DbError> {
    let mut conn = pool.acquire().await?;
    session_policy_in(&mut conn, org_id, user_id, kind, session_id, created_at).await
}

/// Transaction variant, used while the organization is locked before final writes.
pub async fn session_policy_in(
    conn: &mut sqlx::PgConnection,
    org_id: Uuid,
    user_id: Uuid,
    kind: &str,
    session_id: Uuid,
    created_at: OffsetDateTime,
) -> Result<SessionPolicy, DbError> {
    let row: (bool, bool, bool, bool, bool) = sqlx::query_as(
        "SELECT EXISTS(SELECT 1 FROM organization_session_cutoffs c WHERE c.org_id=$1 AND c.user_id=$2 AND c.revoked_before >= $5), \
         COALESCE((SELECT s.enabled AND s.sso_required FROM organization_sso s WHERE s.org_id=$1),false) AND COALESCE((SELECT p.enabled FROM instance_sso_policy p WHERE p.singleton),true), \
         EXISTS(SELECT 1 FROM users u WHERE u.id=$2 AND u.instance_role='operator') OR EXISTS(SELECT 1 FROM organization_members m WHERE m.org_id=$1 AND m.user_id=$2 AND m.status='active' AND m.role='owner'), \
         EXISTS(SELECT 1 FROM sso_session_bindings b WHERE b.org_id=$1 AND b.user_id=$2 AND b.session_kind=$3 AND b.session_id=$4), \
         EXISTS(SELECT 1 FROM sso_session_bindings b JOIN organization_sso s ON s.org_id=b.org_id WHERE b.org_id=$1 AND b.user_id=$2 AND b.session_kind=$3 AND b.session_id=$4 AND b.mfa AND ((b.provider_mfa AND s.trust_mfa) OR (NOT b.provider_mfa AND EXISTS(SELECT 1 FROM user_two_factor f WHERE f.user_id=$2 AND f.enabled_at IS NOT NULL))))"
    ).bind(org_id).bind(user_id).bind(kind).bind(session_id).bind(created_at)
        .fetch_one(conn).await?;
    Ok(if row.0 {
        SessionPolicy::Revoked
    } else if row.1 && !row.2 && !row.3 {
        SessionPolicy::SsoRequired
    } else {
        SessionPolicy::Allowed {
            mfa: row.4,
            sso: row.3,
        }
    })
}

/// Reads the operator's global switch. Absence means the secure documented default.
pub async fn instance_enabled(pool: &PgPool) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT enabled FROM instance_sso_policy WHERE singleton=true",
    )
    .fetch_optional(pool)
    .await?
    .unwrap_or(true))
}

/// Stored provider settings; ciphertext is intentionally absent from diagnostics.
#[derive(sqlx::FromRow)]
pub struct Configuration {
    pub org_id: Uuid,
    pub discovery_url: String,
    pub client_id: String,
    pub client_secret_ciphertext: Vec<u8>,
    pub allowed_domains: Vec<String>,
    pub enabled: bool,
    pub jit_enabled: bool,
    pub sso_required: bool,
    pub trust_mfa: bool,
    pub version: i64,
}
const CONFIG_COLUMNS: &str = "org_id,discovery_url,client_id,client_secret_ciphertext,allowed_domains,enabled,jit_enabled,sso_required,trust_mfa,version";
/// Loads current settings without decrypting credentials.
pub async fn configuration(pool: &PgPool, org: Uuid) -> Result<Option<Configuration>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {CONFIG_COLUMNS} FROM organization_sso WHERE org_id=$1"
    ))
    .bind(org)
    .fetch_optional(pool)
    .await?)
}
/// Reads settings inside an already locked organization transaction.
pub async fn configuration_in(
    conn: &mut sqlx::PgConnection,
    org: Uuid,
) -> Result<Option<Configuration>, DbError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {CONFIG_COLUMNS} FROM organization_sso WHERE org_id=$1"
    ))
    .bind(org)
    .fetch_optional(conn)
    .await?)
}
/// Saves a complete provider configuration; caller owns the organization lock.
///
/// Listed domains become pending claims for the instance operator; removed
/// domains lose their claim. Tightening the sign-in policy revokes live
/// application links the same way an organization-wide two-factor requirement does.
pub async fn save_configuration(
    conn: &mut sqlx::PgConnection,
    c: &Configuration,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    let prior: Option<(bool, bool)> =
        sqlx::query_as("SELECT sso_required,trust_mfa FROM organization_sso WHERE org_id=$1")
            .bind(c.org_id)
            .fetch_optional(&mut *conn)
            .await?;
    sqlx::query("INSERT INTO organization_sso(org_id,discovery_url,client_id,client_secret_ciphertext,allowed_domains,enabled,jit_enabled,sso_required,trust_mfa) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(org_id) DO UPDATE SET discovery_url=EXCLUDED.discovery_url,client_id=EXCLUDED.client_id,client_secret_ciphertext=EXCLUDED.client_secret_ciphertext,allowed_domains=EXCLUDED.allowed_domains,enabled=EXCLUDED.enabled,jit_enabled=EXCLUDED.jit_enabled,sso_required=EXCLUDED.sso_required,trust_mfa=EXCLUDED.trust_mfa,version=organization_sso.version+1,updated_at=now()")
       .bind(c.org_id).bind(&c.discovery_url).bind(&c.client_id).bind(&c.client_secret_ciphertext).bind(&c.allowed_domains)
       .bind(c.enabled).bind(c.jit_enabled).bind(c.sso_required).bind(c.trust_mfa).execute(&mut *conn).await?;
    sqlx::query("DELETE FROM sso_domain_claims WHERE org_id=$1 AND NOT (domain = ANY($2::text[]))")
        .bind(c.org_id)
        .bind(&c.allowed_domains)
        .execute(&mut *conn)
        .await?;
    sqlx::query("INSERT INTO sso_domain_claims(org_id,domain) SELECT $1,d FROM unnest($2::text[]) AS d ON CONFLICT(org_id,domain) DO NOTHING")
        .bind(c.org_id)
        .bind(&c.allowed_domains)
        .execute(&mut *conn)
        .await?;
    // A changed configuration invalidates every pending login immediately.
    sqlx::query("DELETE FROM oidc_transactions WHERE org_id=$1")
        .bind(c.org_id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM oidc_pending_logins WHERE org_id=$1")
        .bind(c.org_id)
        .execute(&mut *conn)
        .await?;
    let (was_required, was_trusting) = prior.unwrap_or((false, false));
    let tightened =
        (c.sso_required && !was_required) || (prior.is_some() && was_trusting && !c.trust_mfa);
    if tightened {
        crate::figcraft::enqueue_org_revocations(
            &mut *conn,
            c.org_id,
            "security_changed",
            OffsetDateTime::now_utc(),
        )
        .await?;
    }
    crate::audit::record_in(
        conn,
        ctx,
        &crate::audit::NewEvent::new("sso.configuration_changed").org(c.org_id),
    )
    .await?;
    Ok(())
}
/// Removes a provider while leaving explicit stable identity history intact.
pub async fn delete_configuration(
    conn: &mut sqlx::PgConnection,
    org: Uuid,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    sqlx::query("DELETE FROM oidc_transactions WHERE org_id=$1")
        .bind(org)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM oidc_pending_logins WHERE org_id=$1")
        .bind(org)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM sso_session_bindings WHERE org_id=$1")
        .bind(org)
        .execute(&mut *conn)
        .await?;
    sqlx::query("DELETE FROM organization_sso WHERE org_id=$1")
        .bind(org)
        .execute(&mut *conn)
        .await?;
    crate::audit::record_in(
        conn,
        ctx,
        &crate::audit::NewEvent::new("sso.configuration_deleted").org(org),
    )
    .await?;
    Ok(())
}
/// Changes the instance switch without altering local recovery credentials.
pub async fn set_instance_enabled(
    pool: &PgPool,
    enabled: bool,
    ctx: &crate::audit::AuditCtx,
) -> Result<(), DbError> {
    let mut tx = pool.begin().await?;
    let authorized:Option<Uuid>=sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND instance_role='operator' AND disabled_at IS NULL FOR SHARE").bind(ctx.actor_id).fetch_optional(&mut *tx).await?;
    if authorized.is_none() {
        return Err(DbError::Internal);
    }

    sqlx::query("INSERT INTO instance_sso_policy(singleton,enabled) VALUES(true,$1) ON CONFLICT(singleton) DO UPDATE SET enabled=EXCLUDED.enabled")
        .bind(enabled).execute(&mut *tx).await?;
    crate::audit::record_in(
        &mut tx,
        ctx,
        &crate::audit::NewEvent::new("sso.instance_policy_changed"),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
/// Finds the organization holding an operator-approved claim on the email domain.
/// A domain is approved for at most one organization; unapproved claims are safe absence.
pub async fn discover_domain(pool: &PgPool, domain: &str) -> Result<Option<Uuid>, DbError> {
    let rows:Vec<Uuid>=sqlx::query_scalar("SELECT s.org_id FROM sso_domain_claims k JOIN organization_sso s ON s.org_id=k.org_id WHERE s.enabled AND k.status='approved' AND lower(k.domain)=lower($1) AND COALESCE((SELECT enabled FROM instance_sso_policy WHERE singleton),true) ORDER BY s.org_id LIMIT 2")
        .bind(domain).fetch_all(pool).await?;
    Ok(if rows.len() == 1 {
        rows.first().copied()
    } else {
        None
    })
}

/// True when the instance operator approved this organization's claim on `domain`.
pub async fn domain_approved(
    conn: &mut sqlx::PgConnection,
    org: Uuid,
    domain: &str,
) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sso_domain_claims WHERE org_id=$1 AND lower(domain)=lower($2) AND status='approved')",
    )
    .bind(org)
    .bind(domain)
    .fetch_one(conn)
    .await?)
}

/// One organization's claim on an email domain.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DomainClaim {
    pub org_id: Uuid,
    pub org_name: String,
    pub domain: String,
    pub status: String,
    pub requested_at: OffsetDateTime,
}

/// Claim states of one organization, domain names sorted.
pub async fn claims_for_org(
    conn: &mut sqlx::PgConnection,
    org: Uuid,
) -> Result<Vec<DomainClaim>, DbError> {
    Ok(sqlx::query_as("SELECT k.org_id,o.name AS org_name,k.domain,k.status,k.requested_at FROM sso_domain_claims k JOIN organizations o ON o.id=k.org_id WHERE k.org_id=$1 ORDER BY k.domain")
        .bind(org).fetch_all(conn).await?)
}

/// Operator view of claims in one state, oldest first.
pub async fn claims_by_status(
    pool: &PgPool,
    status: &str,
    limit: i64,
) -> Result<Vec<DomainClaim>, DbError> {
    Ok(sqlx::query_as("SELECT k.org_id,o.name AS org_name,k.domain,k.status,k.requested_at FROM sso_domain_claims k JOIN organizations o ON o.id=k.org_id WHERE k.status=$1 ORDER BY k.requested_at,k.org_id,k.domain LIMIT $2")
        .bind(status).bind(limit).fetch_all(pool).await?)
}

/// Result of an operator decision on a domain claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimDecision {
    Applied,
    NotFound,
    /// Another organization already holds an approved claim on this domain.
    DomainTaken,
}

/// Approves or rejects a claim. Only an active instance operator may decide;
/// rejecting an approved claim withdraws it. The decision is audited.
pub async fn decide_claim(
    pool: &PgPool,
    org: Uuid,
    domain: &str,
    approve: bool,
    ctx: &crate::audit::AuditCtx,
) -> Result<ClaimDecision, DbError> {
    let mut tx = pool.begin().await?;
    let authorized: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND instance_role='operator' AND disabled_at IS NULL FOR SHARE").bind(ctx.actor_id).fetch_optional(&mut *tx).await?;
    if authorized.is_none() {
        return Err(DbError::Internal);
    }
    let found: Option<String> = sqlx::query_scalar(
        "SELECT status FROM sso_domain_claims WHERE org_id=$1 AND domain=$2 FOR UPDATE",
    )
    .bind(org)
    .bind(domain)
    .fetch_optional(&mut *tx)
    .await?;
    if found.is_none() {
        return Ok(ClaimDecision::NotFound);
    }
    let status = if approve { "approved" } else { "rejected" };
    let updated = sqlx::query("UPDATE sso_domain_claims SET status=$3,decided_at=now(),decided_by=$4 WHERE org_id=$1 AND domain=$2")
        .bind(org).bind(domain).bind(status).bind(ctx.actor_id).execute(&mut *tx).await;
    match updated {
        Ok(_) => {}
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Ok(ClaimDecision::DomainTaken);
        }
        Err(e) => return Err(e.into()),
    }
    crate::audit::record_in(
        &mut tx,
        ctx,
        &crate::audit::NewEvent::new(if approve {
            "sso.domain_approved"
        } else {
            "sso.domain_rejected"
        })
        .org(org),
    )
    .await?;
    tx.commit().await?;
    Ok(ClaimDecision::Applied)
}
