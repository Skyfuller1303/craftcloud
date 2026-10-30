//! Aggregate counts for the organization overview of the admin console.
//!
//! Every function takes an [`OrgScope`] and counts inside SQL, so a response
//! never depends on paging through lists. All queries are bounded by existing
//! indexes: members by the primary key `(org_id, user_id)`, workspaces by
//! `workspaces_org_created_idx`, projects and files by the unique
//! `(org_id, ...)` constraints, grants by `grants_workspace_user_idx`,
//! invitations by `invitations_org_created_idx` and the activity series by
//! the range scan on `audit_events_org_time_idx` over at most 30 days. Locks
//! are few (one row per held file).

use std::collections::BTreeMap;

use sqlx::PgPool;
use time::Date;

use crate::{DbError, OrgScope};

/// Number of UTC days in the activity series (today included).
pub const ACTIVITY_DAYS: i32 = 30;

/// Member counts of an organization.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberCounts {
    /// All memberships by effective status (`active`, `invited`, `disabled`;
    /// a membership of a disabled account counts as `disabled`).
    pub by_status: BTreeMap<String, i64>,
    /// Active members by organization role.
    pub by_role: BTreeMap<String, i64>,
    /// Active members of enabled accounts.
    pub active_members: i64,
    /// Of those, the members with two-factor authentication enabled.
    pub two_factor_enrolled: i64,
}

/// Plain counters of an organization. Files, locks and grants inside a trashed
/// project are not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, sqlx::FromRow)]
pub struct Totals {
    /// Workspaces.
    pub workspaces: i64,
    /// Projects that are not in the trash.
    pub projects: i64,
    /// Files that are not in the trash.
    pub files: i64,
    /// Edit locks that have not expired.
    pub active_locks: i64,
    /// Grants that are not revoked.
    pub active_grants: i64,
    /// Invitations that are neither accepted, revoked, invalidated nor expired.
    pub pending_invitations: i64,
    /// Committed storage in bytes.
    pub committed_bytes: i64,
    /// Reserved (in-flight upload) storage in bytes.
    pub reserved_bytes: i64,
    /// Organization quota in bytes, `None` for unlimited.
    pub quota_bytes: Option<i64>,
}

/// Counts members by status and role and the two-factor coverage.
///
/// # Errors
///
/// Database failures only.
pub async fn member_counts(pool: &PgPool, scope: &OrgScope) -> Result<MemberCounts, DbError> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT CASE WHEN m.status = 'active' AND u.disabled_at IS NOT NULL THEN 'disabled' ELSE m.status END AS status, \
                m.role, count(*), \
                count(*) FILTER (WHERE t.enabled_at IS NOT NULL) \
         FROM organization_members m \
         JOIN users u ON u.id = m.user_id \
         LEFT JOIN user_two_factor t ON t.user_id = m.user_id \
         WHERE m.org_id = $1 \
         GROUP BY 1, 2",
    )
    .bind(scope.org_id())
    .fetch_all(pool)
    .await?;
    let mut counts = MemberCounts::default();
    for status in ["active", "invited", "disabled"] {
        counts.by_status.insert(status.to_owned(), 0);
    }
    for role in ["owner", "admin", "member", "guest"] {
        counts.by_role.insert(role.to_owned(), 0);
    }
    for (status, role, count, enrolled) in rows {
        *counts.by_status.entry(status.clone()).or_insert(0) += count;
        if status == "active" {
            *counts.by_role.entry(role).or_insert(0) += count;
            counts.active_members += count;
            counts.two_factor_enrolled += enrolled;
        }
    }
    Ok(counts)
}

/// Counts workspaces, projects, files, locks, grants and invitations and
/// reads the storage figures of the organization.
///
/// # Errors
///
/// Database failures only. `None` if the organization does not exist.
pub async fn totals(pool: &PgPool, scope: &OrgScope) -> Result<Option<Totals>, DbError> {
    Ok(sqlx::query_as::<_, Totals>(
        "SELECT            (SELECT count(*) FROM workspaces w WHERE w.org_id = o.id) AS workspaces,            (SELECT count(*) FROM projects p WHERE p.org_id = o.id AND p.deleted_at IS NULL) AS projects,            (SELECT count(*) FROM files f WHERE f.org_id = o.id AND f.deleted_at IS NULL \n              AND NOT EXISTS (SELECT 1 FROM projects fp WHERE fp.org_id = f.org_id AND fp.workspace_id = f.workspace_id \n                              AND fp.id = f.project_id AND fp.deleted_at IS NOT NULL)) AS files,            (SELECT count(*) FROM file_locks l JOIN files lf ON lf.org_id = l.org_id AND lf.id = l.file_id               WHERE l.org_id = o.id AND l.expires_at > CURRENT_TIMESTAMP AND lf.app <> 'figcraft' \n                AND NOT EXISTS (SELECT 1 FROM projects lp WHERE lp.org_id = lf.org_id AND lp.workspace_id = lf.workspace_id \n                                AND lp.id = lf.project_id AND lp.deleted_at IS NOT NULL)) AS active_locks,            (SELECT count(*) FROM grants g WHERE g.org_id = o.id AND g.revoked_at IS NULL \n              AND CASE g.target_kind \n                WHEN 'project' THEN NOT EXISTS (SELECT 1 FROM projects gp WHERE gp.org_id = g.org_id AND gp.id = g.target_id \n                                                AND gp.deleted_at IS NOT NULL) \n                ELSE NOT EXISTS (SELECT 1 FROM files gf JOIN projects gfp ON gfp.org_id = gf.org_id \n                                 AND gfp.workspace_id = gf.workspace_id AND gfp.id = gf.project_id \n                                 WHERE gf.org_id = g.org_id AND gf.id = g.target_id \n                                 AND gfp.deleted_at IS NOT NULL) END) AS active_grants,            (SELECT count(*) FROM invitations i WHERE i.org_id = o.id AND i.accepted_at IS NULL               AND i.revoked_at IS NULL AND i.invalidated_at IS NULL AND i.expires_at > CURRENT_TIMESTAMP) AS pending_invitations,            o.committed_bytes, o.reserved_bytes, o.quota_bytes          FROM organizations o WHERE o.id = $1",
    )
    .bind(scope.org_id())
    .fetch_optional(pool)
    .await?)
}

/// Audit events of the organization per UTC day for the last
/// [`ACTIVITY_DAYS`] days, oldest first. Days without events are present with
/// count 0. Aggregated rows count with their `event_count`.
///
/// # Errors
///
/// Database failures only.
pub async fn activity_daily(pool: &PgPool, scope: &OrgScope) -> Result<Vec<(Date, i64)>, DbError> {
    Ok(sqlx::query_as::<_, (Date, i64)>(
        "WITH days AS ( \
           SELECT d::date AS day FROM generate_series( \
             ((CURRENT_TIMESTAMP AT TIME ZONE 'UTC')::date - ($2::int - 1))::timestamp, \
             (CURRENT_TIMESTAMP AT TIME ZONE 'UTC')::date::timestamp, interval '1 day') AS d \
         ), counted AS ( \
           SELECT (created_at AT TIME ZONE 'UTC')::date AS day, sum(event_count)::bigint AS n \
           FROM audit_events \
           WHERE org_id = $1 \
             AND created_at >= (((CURRENT_TIMESTAMP AT TIME ZONE 'UTC')::date - ($2::int - 1))::timestamp AT TIME ZONE 'UTC') \
             AND created_at < (((CURRENT_TIMESTAMP AT TIME ZONE 'UTC')::date + 1)::timestamp AT TIME ZONE 'UTC') \
           GROUP BY 1 \
         ) \
         SELECT days.day, COALESCE(counted.n, 0)::bigint FROM days LEFT JOIN counted USING (day) ORDER BY days.day",
    )
    .bind(scope.org_id())
    .bind(ACTIVITY_DAYS)
    .fetch_all(pool)
    .await?)
}
