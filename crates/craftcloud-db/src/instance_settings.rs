//! Operator-managed instance setup progress and settings (AC-13).

use serde_json::Value;
use sqlx::{PgPool, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::DbError;
use crate::audit::{AuditCtx, Detail, NewEvent, record_in, valid_instance_setting_value};

pub struct SettingMutation {
    pub key: String,
    pub key_code: &'static str,
    pub value: Option<Value>,
    pub secret: bool,
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
    status("system_check") == Some("done")
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

pub async fn update_settings_audited(
    pool: &PgPool,
    actor: Uuid,
    audit: &AuditCtx,
    changes: &[SettingMutation],
) -> Result<(), DbError> {
    let mut ordered = changes.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.key.cmp(&right.key));
    let mut tx = pool.begin().await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('craftcloud.instance.settings', 0))",
    )
    .execute(&mut *tx)
    .await?;
    for change in ordered {
        let old: Option<Value> =
            sqlx::query_scalar("SELECT value FROM instance_settings WHERE key=$1 FOR UPDATE")
                .bind(&change.key)
                .fetch_optional(&mut *tx)
                .await?;
        match &change.value {
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
            if let Some(value) = old.as_ref().and_then(|stored| stored.get("value")) {
                event = event.detail("old", setting_audit_detail(&change.key, value));
            }
            if let Some(value) = change.value.as_ref().and_then(|stored| stored.get("value")) {
                event = event.detail("new", setting_audit_detail(&change.key, value));
            }
        }
        record_in(&mut tx, audit, &event).await?;
    }
    tx.commit().await?;
    Ok(())
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
    fn invalid_legacy_setting_audit_values_are_redacted_without_aborting() {
        let value = serde_json::json!("old-host.example:587");
        assert!(matches!(
            setting_audit_detail("mail.host", &value),
            Detail::Code("legacy_value_unavailable")
        ));
    }
}
