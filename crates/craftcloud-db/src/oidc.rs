//! Tenant-scoped OIDC identity resolution and session issuance.

use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    DbError,
    audit::{AuditCtx, NewEvent, record_in},
    device_sessions::NewDeviceSession,
    two_factor::VerifiedFactor,
};

/// Verified claims from an identity provider. Never include this value in diagnostics.
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub display_name: Option<String>,
    pub mfa: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Resolved(Uuid),
    LinkRequired(Uuid),
    Rejected,
    SeatLimit,
}

/// Resolve a verified provider identity. Caller holds the organization lock.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_identity(
    conn: &mut PgConnection,
    org: Uuid,
    config_version: i64,
    identity: &Identity,
    verified_password_hash: Option<&str>,
    now: OffsetDateTime,
    ctx: &AuditCtx,
) -> Result<Resolution, DbError> {
    if identity.issuer.is_empty()
        || identity.issuer.len() > 2048
        || identity.subject.is_empty()
        || identity.subject.len() > 255
    {
        return Ok(Resolution::Rejected);
    }
    let config: Option<(bool, bool, i64)> = sqlx::query_as(
        "SELECT s.enabled,s.jit_enabled,s.version FROM organization_sso s JOIN organizations o ON o.id=s.org_id WHERE s.org_id=$1 AND COALESCE((SELECT enabled FROM instance_sso_policy WHERE singleton),true)")
        .bind(org).fetch_optional(&mut *conn).await?;
    let Some((true, jit, version)) = config else {
        return Ok(Resolution::Rejected);
    };
    if version != config_version {
        return Ok(Resolution::Rejected);
    }
    let mapped: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM external_identities WHERE issuer=$1 AND subject=$2",
    )
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(user) = mapped {
        // Serialize mapped-login decisions with recovery's user-row lock. Re-read
        // the binding after acquiring it so a pre-recovery lookup cannot survive.
        let enabled: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND disabled_at IS NULL FOR UPDATE)",
        )
        .bind(user)
        .fetch_one(&mut *conn)
        .await?;
        let still_mapped: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM external_identities WHERE issuer=$1 AND subject=$2 AND user_id=$3)",
        )
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .bind(user)
        .fetch_one(&mut *conn)
        .await?;
        if !enabled || !still_mapped {
            return Ok(Resolution::Rejected);
        }
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(org)
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
        if status.as_deref() == Some("active") {
            return Ok(Resolution::Resolved(user));
        }
        if status.is_some() || !jit || !identity.email_verified {
            return Ok(Resolution::Rejected);
        }
        // Only an operator-approved claim makes the provider's email domain meaningful.
        let domain = identity
            .email
            .as_deref()
            .and_then(|e| craftcloud_core::NormalizedEmail::parse(e).ok())
            .and_then(|e| e.as_str().rsplit_once('@').map(|(_, d)| d.to_owned()));
        let allowed = match domain {
            Some(d) => crate::sso::domain_approved(conn, org, &d).await?,
            None => false,
        };
        if !allowed {
            return Ok(Resolution::Rejected);
        }
        if !seat_available(conn, org, now).await? {
            return Ok(Resolution::SeatLimit);
        }
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')").bind(org).bind(user).execute(&mut *conn).await?;
        record_in(
            conn,
            &AuditCtx {
                actor_id: Some(user),
                request_id: ctx.request_id.clone(),
            },
            &NewEvent::new("sso.member_joined")
                .org(org)
                .target("user", user),
        )
        .await?;
        return Ok(Resolution::Resolved(user));
    }
    let Some(email) = identity
        .email
        .as_deref()
        .filter(|_| identity.email_verified)
    else {
        return Ok(Resolution::Rejected);
    };
    let normalized = match craftcloud_core::NormalizedEmail::parse(email) {
        Ok(e) => e.as_str().to_owned(),
        Err(_) => return Ok(Resolution::Rejected),
    };
    if normalized.is_empty() || normalized.len() > 320 {
        return Ok(Resolution::Rejected);
    }
    let domain = normalized.rsplit_once('@').map(|(_, d)| d).unwrap_or("");
    if !crate::sso::domain_approved(conn, org, domain).await? {
        return Ok(Resolution::Rejected);
    }
    // SCIM externalId is explicitly configured as the provider's stable subject.
    // This bridge never follows a directory email change or attaches by email.
    let scim_user:Option<Uuid>=sqlx::query_scalar("SELECT s.user_id FROM scim_users s JOIN organization_members m ON m.org_id=s.org_id AND m.user_id=s.user_id JOIN users u ON u.id=s.user_id WHERE s.org_id=$1 AND s.external_id=$2 AND s.active AND m.status='active' AND m.role='member' AND u.disabled_at IS NULL AND u.password_hash IS NULL AND u.instance_role IS NULL AND u.created_via_invitation_id IS NULL AND NOT EXISTS(SELECT 1 FROM external_identities e WHERE e.user_id=s.user_id) FOR UPDATE OF u")
        .bind(org).bind(&identity.subject).fetch_optional(&mut *conn).await?;
    if let Some(user) = scim_user {
        sqlx::query(
            "INSERT INTO external_identities(org_id,issuer,subject,user_id) VALUES($1,$2,$3,$4)",
        )
        .bind(org)
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .bind(user)
        .execute(&mut *conn)
        .await?;
        record_in(
            conn,
            &AuditCtx {
                actor_id: Some(user),
                request_id: ctx.request_id.clone(),
            },
            &NewEvent::new("sso.account_linked")
                .org(org)
                .target("user", user),
        )
        .await?;
        return Ok(Resolution::Resolved(user));
    }
    let existing: Option<(Uuid, bool, bool, Option<String>, bool)> = sqlx::query_as("SELECT id, disabled_at IS NOT NULL, email_verified_at IS NOT NULL, password_hash, instance_role IS NOT NULL FROM users WHERE normalized_email=$1 FOR UPDATE")
        .bind(&normalized).fetch_optional(&mut *conn).await?;
    if let Some((user, disabled, email_verified, current_hash, instance_account)) = existing {
        // An instance operator account is never linked to a tenant-controlled identity.
        if disabled || instance_account {
            return Ok(Resolution::Rejected);
        }
        if current_hash.is_none() {
            return Ok(Resolution::Rejected);
        }
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM organization_members WHERE org_id=$1 AND user_id=$2",
        )
        .bind(org)
        .bind(user)
        .fetch_optional(&mut *conn)
        .await?;
        if status.as_deref().is_some_and(|s| s != "active") || (status.is_none() && !jit) {
            return Ok(Resolution::Rejected);
        }
        if !email_verified {
            return Ok(Resolution::Rejected);
        }
        let Some(password_hash) = verified_password_hash else {
            return Ok(Resolution::LinkRequired(user));
        };
        let proof: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1 AND password_hash=$2 AND disabled_at IS NULL FOR UPDATE)")
            .bind(user).bind(password_hash).fetch_one(&mut *conn).await?;
        if !proof {
            return Ok(Resolution::Rejected);
        }
        if status.is_none() {
            if !seat_available(conn, org, now).await? {
                return Ok(Resolution::SeatLimit);
            }
            sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')").bind(org).bind(user).execute(&mut *conn).await?;
        }
        sqlx::query(
            "INSERT INTO external_identities(org_id,issuer,subject,user_id) VALUES($1,$2,$3,$4)",
        )
        .bind(org)
        .bind(&identity.issuer)
        .bind(&identity.subject)
        .bind(user)
        .execute(&mut *conn)
        .await?;
        record_in(
            conn,
            &AuditCtx {
                actor_id: Some(user),
                request_id: ctx.request_id.clone(),
            },
            &NewEvent::new("sso.account_linked")
                .org(org)
                .target("user", user),
        )
        .await?;
        return Ok(Resolution::Resolved(user));
    }
    if !jit {
        return Ok(Resolution::Rejected);
    }
    if !seat_available(conn, org, now).await? {
        return Ok(Resolution::SeatLimit);
    }
    let user = Uuid::new_v4();
    let fallback = normalized.chars().take(100).collect::<String>();
    let display = identity
        .display_name
        .as_deref()
        .filter(|s| !s.trim().is_empty() && s.chars().count() <= 100)
        .unwrap_or(&fallback);
    sqlx::query(
        "INSERT INTO users(id,normalized_email,display_name,email_verified_at) VALUES($1,$2,$3,$4)",
    )
    .bind(user)
    .bind(&normalized)
    .bind(display)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
        .bind(org).bind(user).execute(&mut *conn).await?;
    sqlx::query(
        "INSERT INTO external_identities(org_id,issuer,subject,user_id) VALUES($1,$2,$3,$4)",
    )
    .bind(org)
    .bind(&identity.issuer)
    .bind(&identity.subject)
    .bind(user)
    .execute(&mut *conn)
    .await?;
    record_in(
        conn,
        &AuditCtx {
            actor_id: Some(user),
            request_id: ctx.request_id.clone(),
        },
        &NewEvent::new("sso.account_created")
            .org(org)
            .target("user", user),
    )
    .await?;
    Ok(Resolution::Resolved(user))
}

pub enum SessionInput {
    Browser {
        token_hash: [u8; 32],
        absolute_expires_at: OffsetDateTime,
    },
    Device(NewDeviceSession),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    Created(Uuid),
    FactorRequired,
    SetupRequired,
    Rejected,
    AppNotAssigned,
}

/// Issue one SSO session and bind its provenance in the caller's transaction.
#[allow(clippy::too_many_arguments)]
pub async fn issue_session(
    conn: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    org: Uuid,
    user: Uuid,
    config_version: i64,
    identity_mfa: bool,
    local_factor: Option<&VerifiedFactor>,
    verified_factor_ciphertext: Option<&[u8]>,
    session: &SessionInput,
    now: OffsetDateTime,
    ctx: &AuditCtx,
    mail_enabled: bool,
) -> Result<LoginOutcome, DbError> {
    let locked: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 AND disabled_at IS NULL FOR UPDATE")
            .bind(user)
            .fetch_optional(&mut **conn)
            .await?;
    if locked.is_none() {
        return Ok(LoginOutcome::Rejected);
    }
    let config: Option<(bool,bool,bool,i64)> = sqlx::query_as("SELECT s.enabled,s.trust_mfa,o.require_two_factor,s.version FROM organization_sso s JOIN organizations o ON o.id=s.org_id WHERE s.org_id=$1 AND COALESCE((SELECT enabled FROM instance_sso_policy WHERE singleton),true)")
        .bind(org).fetch_optional(&mut **conn).await?;
    let Some((true, trust_mfa, require_factor, version)) = config else {
        return Ok(LoginOutcome::Rejected);
    };
    if version != config_version {
        return Ok(LoginOutcome::Rejected);
    }
    let member: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM organization_members WHERE org_id=$1 AND user_id=$2 AND status='active')")
        .bind(org).bind(user).fetch_one(&mut **conn).await?;
    if !member {
        return Ok(LoginOutcome::Rejected);
    }
    if let SessionInput::Device(device) = session
        && device.client_kind == "app"
    {
        if !device.known_app_ids.contains(&device.client_id) {
            return Ok(LoginOutcome::AppNotAssigned);
        }
        let allowed: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM organization_members m WHERE m.user_id=$1 AND m.org_id=$2 AND m.status='active') OR EXISTS(SELECT 1 FROM organization_members m JOIN organizations o ON o.id=m.org_id WHERE m.user_id=$1 AND m.org_id=$2 AND m.status='active' AND (o.app_access_mode='all' OR (($3=ANY($4::text[])) AND (m.role IN ('owner','admin','guest') OR EXISTS(SELECT 1 FROM member_app_access a WHERE a.org_id=m.org_id AND a.user_id=m.user_id AND a.app=$3)))))")
                .bind(user).bind(org).bind(&device.client_id).bind(&device.known_app_ids).fetch_one(&mut **conn).await?;
        if !allowed {
            return Ok(LoginOutcome::AppNotAssigned);
        }
    }
    let configured_factor: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)",
    )
    .bind(user)
    .fetch_one(&mut **conn)
    .await?;
    // Provider MFA only counts for this organization and only for accounts without a
    // local factor: it never replaces a configured TOTP factor or its lockout.
    let provider_factor = trust_mfa && identity_mfa && !configured_factor;
    let locked_factor: bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts >= $2)").bind(user).bind(crate::two_factor::MAX_FAILED_ATTEMPTS).fetch_one(&mut **conn).await?;
    if locked_factor {
        return Ok(LoginOutcome::Rejected);
    }

    if configured_factor && local_factor.is_none() {
        return Ok(LoginOutcome::FactorRequired);
    }
    if require_factor && !provider_factor && !configured_factor {
        return Ok(LoginOutcome::SetupRequired);
    }
    if let Some(factor) = local_factor {
        if !configured_factor {
            return Ok(LoginOutcome::Rejected);
        }
        let secret: Option<Vec<u8>> = sqlx::query_scalar("SELECT secret_ciphertext FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL AND failed_attempts<$2 FOR UPDATE")
            .bind(user).bind(crate::two_factor::MAX_FAILED_ATTEMPTS).fetch_optional(&mut **conn).await?;
        if secret
            .as_deref()
            .zip(verified_factor_ciphertext)
            .is_none_or(|(actual, expected)| actual != expected)
        {
            return Ok(LoginOutcome::Rejected);
        }
        let used = match factor {
            VerifiedFactor::TotpStep(step) => sqlx::query("UPDATE user_two_factor SET last_accepted_step=$2,failed_attempts=0 WHERE user_id=$1 AND enabled_at IS NOT NULL AND (last_accepted_step IS NULL OR last_accepted_step<$2)").bind(user).bind(*step).execute(&mut **conn).await?.rows_affected()==1,
            VerifiedFactor::RecoverySalt(salt) => sqlx::query("UPDATE user_two_factor_recovery_codes SET used_at=$3 WHERE user_id=$1 AND salt=$2 AND used_at IS NULL AND EXISTS(SELECT 1 FROM user_two_factor WHERE user_id=$1 AND enabled_at IS NOT NULL)").bind(user).bind(salt.as_slice()).bind(now).execute(&mut **conn).await?.rows_affected()==1,
        };
        if !used {
            return Ok(LoginOutcome::Rejected);
        }
        sqlx::query(
            "UPDATE user_two_factor SET failed_attempts=0 WHERE user_id=$1 AND failed_attempts<>0",
        )
        .bind(user)
        .execute(&mut **conn)
        .await?;
    }
    let id = Uuid::new_v4();
    let (kind, mfa) = match session {
        SessionInput::Browser {
            token_hash,
            absolute_expires_at,
        } => {
            sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)")
                .bind(id).bind(user).bind(token_hash.as_slice()).bind(now).bind(absolute_expires_at).execute(&mut **conn).await?;
            ("browser", provider_factor || local_factor.is_some())
        }
        SessionInput::Device(d) => {
            let family = Uuid::new_v4();
            sqlx::query("INSERT INTO device_sessions(id,user_id,name,client_kind,client_id,refresh_hash,refresh_family,access_hash,access_expires_at,created_at,last_used_at,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,$11,$12)")
                .bind(id).bind(user).bind(&d.name).bind(&d.client_kind).bind(&d.client_id).bind(d.refresh_hash.as_slice()).bind(family).bind(d.access_hash.as_slice()).bind(d.access_expires_at).bind(now).bind(d.idle_expires_at).bind(d.absolute_expires_at).execute(&mut **conn).await?;
            ("device", provider_factor || local_factor.is_some())
        }
    };
    sqlx::query("INSERT INTO sso_session_bindings(session_kind,session_id,org_id,user_id,mfa,provider_mfa) VALUES($1,$2,$3,$4,$5,$6)")
        .bind(kind).bind(id).bind(org).bind(user).bind(mfa).bind(provider_factor).execute(&mut **conn).await?;
    let audit = AuditCtx {
        actor_id: Some(user),
        request_id: ctx.request_id.clone(),
    };
    record_in(
        conn,
        &audit,
        &NewEvent::new("sso.login").org(org).target(
            if kind == "browser" {
                "session"
            } else {
                "device_session"
            },
            id,
        ),
    )
    .await?;
    if mail_enabled {
        crate::mail_security::enqueue_in(
            conn,
            user,
            now,
            crate::mail_security::SecurityEvent::NewLogin,
        )
        .await?;
    }
    Ok(LoginOutcome::Created(id))
}

async fn seat_available(
    conn: &mut PgConnection,
    org: Uuid,
    now: OffsetDateTime,
) -> Result<bool, DbError> {
    let capacity: (Option<i32>, i64) = sqlx::query_as("SELECT o.seat_limit,(SELECT count(*) FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=o.id AND m.status='active' AND m.role<>'guest' AND u.disabled_at IS NULL) + (SELECT count(*) FROM invitations i LEFT JOIN users u ON u.normalized_email=i.normalized_email LEFT JOIN organization_members m ON m.org_id=i.org_id AND m.user_id=u.id WHERE i.org_id=o.id AND i.org_role<>'guest' AND i.accepted_at IS NULL AND i.revoked_at IS NULL AND i.invalidated_at IS NULL AND i.expires_at>$2 AND (m.user_id IS NULL OR m.status<>'active')) FROM organizations o WHERE o.id=$1")
        .bind(org).bind(now).fetch_one(&mut *conn).await?;
    if capacity
        .0
        .is_some_and(|limit| capacity.1 >= i64::from(limit))
    {
        return Ok(false);
    }
    Ok(true)
}
