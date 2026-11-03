//! Operator-managed instance setup progress and settings (AC-13).

use serde_json::Value;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent, record_in, valid_instance_setting_value};

/// Encrypted secret keys. They never appear in audit values or API responses.
pub const SECRET_KEYS: &[&str] = &[
    "mail.password",
    "mail.client_secret",
    "mail.certificate",
    "mail.private_key",
    "mail.service_account",
];

/// Keys that decide which remote endpoint or identity receives stored
/// credentials. Changing any of them invalidates every stored secret.
pub const CREDENTIAL_TARGET_KEYS: &[&str] = &[
    "mail.transport",
    "mail.host",
    "mail.port",
    "mail.tls",
    "mail.tenant_id",
    "mail.client_id",
];

/// After a retention period is shortened, deletion, purge and restore keep
/// using the longer of the old and the new period for this long.
pub const RETENTION_GRACE: time::Duration = time::Duration::hours(24);

pub struct SettingMutation {
    pub key: String,
    pub key_code: &'static str,
    pub value: Option<Value>,
    pub secret: bool,
    /// Effective value while no row is stored (default or configured
    /// fallback). Used to detect real changes and shortened retention.
    pub baseline: Option<Value>,
}

fn valid_retention_days(key: &str, days: u64) -> bool {
    if key == "retention.audit_days" {
        (30..=3650).contains(&days)
    } else {
        (1..=3650).contains(&days)
    }
}

/// Effective retention in days for a stored retention row. During the grace
/// period after a shortening this is the longer of the new and the previous
/// period. Returns `None` when the stored value is not a valid period, so the
/// caller can fall back to the environment or default value.
#[must_use]
pub fn effective_retention_days(
    key: &str,
    stored: &Value,
    updated_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Option<u64> {
    let value = stored
        .get("value")
        .and_then(Value::as_u64)
        .filter(|days| valid_retention_days(key, *days))?;
    let previous = stored
        .get("previous")
        .and_then(Value::as_u64)
        .filter(|days| valid_retention_days(key, *days))
        .filter(|_| now - updated_at < RETENTION_GRACE);
    Some(previous.map_or(value, |previous| previous.max(value)))
}

/// Effective retention days from stored rows. Invalid rows are ignored and
/// logged by key only.
#[must_use]
pub fn retention_days_from_rows(
    rows: &[InstanceSetting],
    key: &str,
    now: OffsetDateTime,
) -> Option<u64> {
    let row = rows.iter().find(|row| row.key == key)?;
    let days = effective_retention_days(key, &row.value, row.updated_at, now);
    if days.is_none() {
        tracing::warn!(setting = key, "ignoring invalid stored retention setting");
    }
    days
}

fn valid_version_setting(key: &str, value: u64) -> bool {
    match key {
        "versions.keep_all_hours" => value <= 168,
        "versions.daily_days" => value <= 90,
        "versions.weekly_weeks" => value <= 52,
        "versions.max_per_file" => value == 0 || (2..=10_000).contains(&value),
        _ => false,
    }
}

/// Effective value for a thinning setting, including the 24-hour grace after
/// the setting was shortened.
#[must_use]
pub fn effective_version_setting(
    key: &str,
    stored: &Value,
    updated_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Option<u64> {
    let value = stored
        .get("value")
        .and_then(Value::as_u64)
        .filter(|value| valid_version_setting(key, *value))?;
    let previous = stored
        .get("previous")
        .and_then(Value::as_u64)
        .filter(|previous| valid_version_setting(key, *previous))
        .filter(|_| now - updated_at < RETENTION_GRACE);
    Some(previous.map_or(value, |previous| {
        if key == "versions.max_per_file" && previous == 0 {
            0
        } else {
            previous.max(value)
        }
    }))
}

/// Reads a valid thinning setting from stored rows.
#[must_use]
pub fn version_setting_from_rows(
    rows: &[InstanceSetting],
    key: &str,
    now: OffsetDateTime,
) -> Option<u64> {
    let row = rows.iter().find(|row| row.key == key)?;
    let value = effective_version_setting(key, &row.value, row.updated_at, now);
    if value.is_none() {
        tracing::warn!(setting = key, "ignoring invalid stored version setting");
    }
    value
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepUpdateError {
    Db(DbError),
    AlreadyComplete,
    Incomplete,
}

const REQUIRED_STEPS: &[&str] = &[
    "two_factor",
    "email_delivery",
    "organization",
    "workspace_project",
    "invitations",
    "optional_services",
];

fn rows_can_finish(rows: &[(String, String)]) -> bool {
    let status = |step: &str| {
        rows.iter()
            .find(|(id, _)| id == step)
            .map(|(_, status)| status.as_str())
    };
    matches!(status("system_check"), Some("done" | "skipped"))
        && REQUIRED_STEPS
            .iter()
            .all(|step| matches!(status(step), Some("done" | "skipped")))
}

fn rows_complete(rows: &[(String, String)]) -> bool {
    rows_can_finish(rows)
        && rows
            .iter()
            .any(|(step, status)| step == "summary" && status == "done")
}

fn setting_audit_detail(key: &str, value: &Value) -> Detail {
    if valid_instance_setting_value(key, value) {
        Detail::InstanceSetting {
            key: key.to_owned(),
            value: value.clone(),
        }
    } else {
        Detail::Code("legacy_value_unavailable")
    }
}

impl From<sqlx::Error> for StepUpdateError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(DbError::from(error))
    }
}

impl From<DbError> for StepUpdateError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

#[derive(Debug, Clone)]
pub struct SetupStep {
    pub step: String,
    pub status: String,
    pub updated_at: OffsetDateTime,
    pub updated_by: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct InstanceSetting {
    pub key: String,
    pub value: Value,
    pub updated_at: OffsetDateTime,
    pub updated_by: Option<Uuid>,
}

pub async fn setup_steps(pool: &PgPool) -> Result<Vec<SetupStep>, DbError> {
    let rows = sqlx::query(
        "SELECT step, status, updated_at, updated_by FROM instance_setup_steps ORDER BY step",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(SetupStep {
                step: row.try_get("step")?,
                status: row.try_get("status")?,
                updated_at: row.try_get("updated_at")?,
                updated_by: row.try_get("updated_by")?,
            })
        })
        .collect()
}

pub async fn set_setup_step(
    pool: &PgPool,
    step: &str,
    status: &str,
    actor: Uuid,
) -> Result<SetupStep, DbError> {
    let row = sqlx::query(
        "INSERT INTO instance_setup_steps (step, status, updated_by) VALUES ($1, $2, $3) \
         ON CONFLICT (step) DO UPDATE SET status = EXCLUDED.status, \
         updated_at = now(), updated_by = EXCLUDED.updated_by \
         RETURNING step, status, updated_at, updated_by",
    )
    .bind(step)
    .bind(status)
    .bind(actor)
    .fetch_one(pool)
    .await?;
    Ok(SetupStep {
        step: row.try_get("step")?,
        status: row.try_get("status")?,
        updated_at: row.try_get("updated_at")?,
        updated_by: row.try_get("updated_by")?,
    })
}

pub async fn set_setup_step_audited(
    pool: &PgPool,
    step: &str,
    step_code: &'static str,
    status: &str,
    actor: Uuid,
    audit: &AuditCtx,
) -> Result<SetupStep, StepUpdateError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('craftcloud.setup.steps', 0))")
        .execute(&mut *tx)
        .await?;
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT step,status FROM instance_setup_steps FOR UPDATE",
    )
    .fetch_all(&mut *tx)
    .await?;
    let setup_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM instance_setup WHERE id=true)")
            .fetch_one(&mut *tx)
            .await?;
    let complete = setup_exists && rows_complete(&rows);
    if complete {
        return Err(StepUpdateError::AlreadyComplete);
    }
    if step == "summary" && status != "done" {
        return Err(StepUpdateError::Db(DbError::Internal));
    }
    if step == "summary" && (!setup_exists || !rows_can_finish(&rows)) {
        return Err(StepUpdateError::Incomplete);
    }
    let row = sqlx::query(
        "INSERT INTO instance_setup_steps (step, status, updated_by) VALUES ($1, $2, $3) \
         ON CONFLICT (step) DO UPDATE SET status = EXCLUDED.status, \
         updated_at = now(), updated_by = EXCLUDED.updated_by \
         RETURNING step, status, updated_at, updated_by",
    )
    .bind(step)
    .bind(status)
    .bind(actor)
    .fetch_one(&mut *tx)
    .await?;
    let result = SetupStep {
        step: row.try_get("step")?,
        status: row.try_get("status")?,
        updated_at: row.try_get("updated_at")?,
        updated_by: row.try_get("updated_by")?,
    };
    let action = if status == "done" {
        "setup.step.completed"
    } else {
        "setup.step.skipped"
    };
    record_in(
        &mut tx,
        audit,
        &NewEvent::new(action).detail("step", Detail::Code(step_code)),
    )
    .await?;
    tx.commit().await?;
    Ok(result)
}

pub async fn settings(pool: &PgPool) -> Result<Vec<InstanceSetting>, DbError> {
    let rows = sqlx::query(
        "SELECT key, value, updated_at, updated_by FROM instance_settings ORDER BY key",
    )
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok(InstanceSetting {
                key: row.try_get("key")?,
                value: row.try_get("value")?,
                updated_at: row.try_get("updated_at")?,
                updated_by: row.try_get("updated_by")?,
            })
        })
        .collect()
}

pub async fn set_setting(
    pool: &PgPool,
    key: &str,
    value: &Value,
    actor: Uuid,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO instance_settings (key, value, updated_by) VALUES ($1, $2, $3) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, \
         updated_at = now(), updated_by = EXCLUDED.updated_by",
    )
    .bind(key)
    .bind(value)
    .bind(actor)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_setting(pool: &PgPool, key: &str) -> Result<(), DbError> {
    sqlx::query("DELETE FROM instance_settings WHERE key = $1")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Applies the settings changes in one transaction and returns the secret keys
/// that were deleted because a credential target changed.
pub async fn update_settings_audited(
    pool: &PgPool,
    actor: Uuid,
    audit: &AuditCtx,
    changes: &[SettingMutation],
    now: OffsetDateTime,
) -> Result<Vec<String>, DbError> {
    let mut ordered = changes.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.key.cmp(&right.key));
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('craftcloud.instance.settings', 0))",
    )
    .execute(&mut *tx)
    .await?;
    let mut olds = Vec::with_capacity(ordered.len());
    for change in &ordered {
        let old: Option<(Value, OffsetDateTime)> = sqlx::query_as(
            "SELECT value, updated_at FROM instance_settings WHERE key=$1 FOR UPDATE",
        )
        .bind(&change.key)
        .fetch_optional(&mut *tx)
        .await?;
        olds.push(old);
    }
    // Removing a stored target value also changes the destination (it falls
    // back to the default), so it counts as a change too.
    let target_changed = ordered.iter().zip(&olds).any(|(change, old)| {
        if !CREDENTIAL_TARGET_KEYS.contains(&change.key.as_str()) {
            return false;
        }
        let previous = old
            .as_ref()
            .and_then(|(stored, _)| stored.get("value"))
            .or(change.baseline.as_ref())
            .filter(|value| !value.is_null());
        match change.value.as_ref() {
            Some(new) => previous != new.get("value").filter(|value| !value.is_null()),
            None => old.is_some(),
        }
    });
    for (change, old) in ordered.iter().zip(&olds) {
        let mut stored = change.value.clone();
        if (change.key.starts_with("retention.") || change.key.starts_with("versions."))
            && let Some(new_days) = stored
                .as_ref()
                .and_then(|value| value.get("value"))
                .and_then(Value::as_u64)
        {
            let old_days = old
                .as_ref()
                .and_then(|(value, updated_at)| {
                    if change.key.starts_with("versions.") {
                        effective_version_setting(&change.key, value, *updated_at, now)
                    } else {
                        effective_retention_days(&change.key, value, *updated_at, now)
                    }
                })
                .or_else(|| change.baseline.as_ref().and_then(Value::as_u64));
            let shortened = old_days.is_some_and(|old_days| {
                if change.key == "versions.max_per_file" {
                    old_days == 0 && new_days > 0
                        || old_days > 0 && new_days > 0 && new_days < old_days
                } else {
                    new_days < old_days
                }
            });
            if shortened && let Some(old_days) = old_days {
                stored = Some(serde_json::json!({"value": new_days, "previous": old_days}));
            }
        }
        match &stored {
            Some(value) => {
                sqlx::query("INSERT INTO instance_settings(key,value,updated_by) VALUES($1,$2,$3) \
                    ON CONFLICT(key) DO UPDATE SET value=EXCLUDED.value,updated_at=now(),updated_by=EXCLUDED.updated_by")
                    .bind(&change.key).bind(value).bind(actor).execute(&mut *tx).await?;
            }
            None => {
                sqlx::query("DELETE FROM instance_settings WHERE key=$1")
                    .bind(&change.key)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        let mut event =
            NewEvent::new("instance_setting.updated").detail("key", Detail::Code(change.key_code));
        if !change.secret {
            if let Some(value) = old.as_ref().and_then(|(stored, _)| stored.get("value")) {
                event = event.detail("old", setting_audit_detail(&change.key, value));
            }
            if let Some(value) = stored.as_ref().and_then(|stored| stored.get("value")) {
                event = event.detail("new", setting_audit_detail(&change.key, value));
            }
        }
        record_in(&mut tx, audit, &event).await?;
    }
    let mut cleared = Vec::new();
    if target_changed {
        for key in SECRET_KEYS {
            if ordered
                .iter()
                .any(|change| change.key == *key && change.value.is_some())
            {
                continue;
            }
            let removed = sqlx::query("DELETE FROM instance_settings WHERE key=$1")
                .bind(key)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if removed > 0 {
                record_in(
                    &mut tx,
                    audit,
                    &NewEvent::new("instance_setting.secret_cleared")
                        .detail("key", Detail::Code(key))
                        .detail("reason", Detail::Code("credential_target_changed")),
                )
                .await?;
                cleared.push((*key).to_owned());
            }
        }
    }
    tx.commit().await?;
    Ok(cleared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_cannot_finish_until_every_required_step_is_resolved() {
        let mut rows = vec![("system_check".into(), "done".into())];
        rows.extend(
            REQUIRED_STEPS
                .iter()
                .map(|step| ((*step).into(), "done".into())),
        );
        assert!(rows_can_finish(&rows));
        assert!(!rows_complete(&rows));
        rows.push(("summary".into(), "done".into()));
        assert!(rows_complete(&rows));
        rows.retain(|(step, _)| step != "invitations");
        assert!(!rows_can_finish(&rows));
    }

    #[test]
    fn retention_grace_uses_the_longer_period_for_24_hours() {
        let updated = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let stored = serde_json::json!({"value": 7, "previous": 30});
        let key = "retention.trash_days";
        assert_eq!(
            effective_retention_days(key, &stored, updated, updated + time::Duration::hours(23)),
            Some(30)
        );
        assert_eq!(
            effective_retention_days(key, &stored, updated, updated + time::Duration::hours(24)),
            Some(7)
        );
        assert_eq!(
            effective_retention_days(key, &serde_json::json!({"value": 0}), updated, updated),
            None
        );
        assert_eq!(
            effective_retention_days(
                "retention.audit_days",
                &serde_json::json!({"value": 29}),
                updated,
                updated
            ),
            None
        );
        assert_eq!(
            effective_retention_days(key, &serde_json::json!({"value": "x"}), updated, updated),
            None
        );
    }

    #[test]
    fn thinning_grace_preserves_unlimited_cap_until_expiry() {
        let updated = OffsetDateTime::from_unix_timestamp(1_000_000).unwrap();
        let stored = serde_json::json!({"value": 100, "previous": 0});
        assert_eq!(
            effective_version_setting(
                "versions.max_per_file",
                &stored,
                updated,
                updated + time::Duration::hours(23)
            ),
            Some(0)
        );
        assert_eq!(
            effective_version_setting(
                "versions.max_per_file",
                &stored,
                updated,
                updated + time::Duration::hours(24)
            ),
            Some(100)
        );
    }

    #[test]
    fn invalid_legacy_setting_audit_values_are_redacted_without_aborting() {
        let value = serde_json::json!("old-host.example:587");
        assert!(matches!(
            setting_audit_detail("mail.host", &value),
            Detail::Code("legacy_value_unavailable")
        ));
    }
}
