//! AC-11 seat-limit integration tests.
//!
//! Run through `cargo xtask db-test`; each test uses its own PostgreSQL database.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;
use std::time::Duration;

use craftcloud_core::policy::{
    Action, Actor, InvitationSpec, OrgMembership, OrgRole, Resource, can,
};
use craftcloud_db::accounts::{self, DisableOutcome};
use craftcloud_db::invitations::{
    AcceptBy, AcceptOutcome, CreateInvitationOutcome, NewInvitation, accept,
};
use craftcloud_db::members::{AddOutcome, OrgTx, RoleOutcome};
use craftcloud_db::migrate;
use craftcloud_db::organizations::{self, MemberStatus, NewOrganization};
use craftcloud_db::users::{self, NewUser, User};
use craftcloud_db::{OrgScope, PgPool};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection};
use uuid::Uuid;

const ENV: &str = "CC_TEST_DATABASE_URL";

struct TestDb {
    name: String,
    admin: PgConnectOptions,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let admin_url = std::env::var(ENV).expect("CC_TEST_DATABASE_URL must be set");
        let admin = PgConnectOptions::from_str(&admin_url).unwrap();
        let name = format!("cc_test_{}", Uuid::new_v4().simple());
        let mut conn = PgConnection::connect_with(&admin).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(12)
            .acquire_timeout(Duration::from_secs(10))
            .connect_with(admin.clone().database(&name))
            .await
            .unwrap();
        migrate::run(&pool).await.unwrap();
        Self { name, admin, pool }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                    let _ = conn.close().await;
                }
            });
        })
        .join();
    }
}

async fn user(pool: &PgPool, email: &str) -> User {
    users::insert(
        pool,
        &NewUser {
            normalized_email: email.to_owned(),
            display_name: "Test User".to_owned(),
        },
    )
    .await
    .unwrap()
}

async fn org(pool: &PgPool, slug: &str, limit: i32) -> (Uuid, OrgScope, User) {
    let owner = user(pool, &format!("owner-{slug}@example.test")).await;
    let created = organizations::create(
        pool,
        &NewOrganization {
            slug: slug.to_owned(),
            name: format!("Org {slug}"),
            owner_user_id: owner.id,
        },
    )
    .await
    .unwrap();
    organizations::add_member(
        pool,
        &member_add_scope(created.id),
        owner.id,
        OrgRole::Owner,
        MemberStatus::Active,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE organizations SET seat_limit=$2 WHERE id=$1")
        .bind(created.id)
        .bind(limit)
        .execute(pool)
        .await
        .unwrap();
    (created.id, org_scope(created.id), owner)
}

fn org_scope(org_id: Uuid) -> OrgScope {
    let actor = Actor {
        user_id: Uuid::new_v4(),
        instance_operator: false,
        membership: Some(OrgMembership {
            org_id,
            role: OrgRole::Owner,
        }),
        workspace: None,
    };
    let permit = can(
        &actor,
        Action::OrganizationUpdate,
        &Resource::Organization { org_id },
    )
    .permit()
    .unwrap();
    OrgScope::from_permit(&permit).unwrap()
}

fn member_add_scope(org_id: Uuid) -> OrgScope {
    let actor = Actor {
        user_id: Uuid::new_v4(),
        instance_operator: false,
        membership: Some(OrgMembership {
            org_id,
            role: OrgRole::Owner,
        }),
        workspace: None,
    };
    let permit = can(
        &actor,
        Action::MemberAdd,
        &Resource::Member {
            org_id,
            target: None,
            role: Some(OrgRole::Member),
        },
    )
    .permit()
    .unwrap();
    OrgScope::from_permit(&permit).unwrap()
}

fn role_change_scope(org_id: Uuid, user_id: Uuid) -> OrgScope {
    let actor = Actor {
        user_id: Uuid::new_v4(),
        instance_operator: false,
        membership: Some(OrgMembership {
            org_id,
            role: OrgRole::Owner,
        }),
        workspace: None,
    };
    let permit = can(
        &actor,
        Action::MemberChangeRole,
        &Resource::Member {
            org_id,
            target: Some(craftcloud_core::policy::MemberTarget {
                user_id,
                role: OrgRole::Guest,
            }),
            role: Some(OrgRole::Member),
        },
    )
    .permit()
    .unwrap();
    OrgScope::from_permit(&permit).unwrap()
}

fn invitation_scope(org_id: Uuid, inviter: Uuid) -> OrgScope {
    let actor = Actor {
        user_id: inviter,
        instance_operator: false,
        membership: Some(OrgMembership {
            org_id,
            role: OrgRole::Owner,
        }),
        workspace: None,
    };
    let permit = can(
        &actor,
        Action::InvitationCreate,
        &Resource::Invitation {
            org_id,
            spec: Some(InvitationSpec {
                org_role: OrgRole::Member,
                workspace: None,
                guest_workspace: None,
                workspace_admin_invites_enabled: false,
                created_by: Some(inviter),
            }),
        },
    )
    .permit()
    .unwrap();
    OrgScope::from_permit(&permit).unwrap()
}

fn invitation(
    email: String,
    inviter: Uuid,
    now: time::OffsetDateTime,
    role: OrgRole,
) -> NewInvitation {
    let token_hash = {
        let first = *Uuid::new_v4().as_bytes();
        let second = *Uuid::new_v4().as_bytes();
        let mut hash = [0; 32];
        hash[..16].copy_from_slice(&first);
        hash[16..].copy_from_slice(&second);
        hash
    };
    NewInvitation {
        email,
        org_role: role,
        workspace: None,
        guest_workspace: None,
        token_hash,
        language: "en".to_owned(),
        email_delivery: false,
        created_by: inviter,
        now,
        expires_at: now + time::Duration::days(7),
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn concurrent_non_guest_invites_reserve_only_available_seat() {
    let db = TestDb::new().await;
    let (org_id, _, owner) = org(&db.pool, "seat-race", 2).await;
    let scope = invitation_scope(org_id, owner.id);
    let now = time::OffsetDateTime::now_utc();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..8 {
        let pool = db.pool.clone();
        let email = format!("racer-{index}@example.test");
        let new = invitation(email, owner.id, now, OrgRole::Member);
        tasks.spawn(async move {
            let mut tx = OrgTx::begin(&pool, org_id).await.unwrap().unwrap();
            let outcome = tx.invitation_create(&scope, &new).await.unwrap();
            tx.commit().await.unwrap();
            outcome
        });
    }
    let mut outcomes = Vec::new();
    while let Some(result) = tasks.join_next().await {
        outcomes.push(result.unwrap());
    }
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CreateInvitationOutcome::Created(_)))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, CreateInvitationOutcome::SeatLimitReached))
            .count(),
        7
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn guests_do_not_consume_seats_and_reactivation_or_promotion_is_limited() {
    let db = TestDb::new().await;
    let (org_id, _, owner) = org(&db.pool, "seat-guest", 1).await;
    let guest = user(&db.pool, "guest-seat@example.test").await;
    let guest_scope = member_add_scope(org_id);
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.add(&guest_scope, guest.id, OrgRole::Guest)
            .await
            .unwrap(),
        AddOutcome::Added(_)
    ));
    tx.commit().await.unwrap();

    let reactivating = user(&db.pool, "reactivate-seat@example.test").await;
    organizations::add_member(
        &db.pool,
        &guest_scope,
        reactivating.id,
        OrgRole::Member,
        MemberStatus::Disabled,
    )
    .await
    .unwrap();
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.add(&guest_scope, reactivating.id, OrgRole::Member)
            .await
            .unwrap(),
        AddOutcome::SeatLimitReached
    ));
    tx.commit().await.unwrap();

    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.set_role(
            &role_change_scope(org_id, guest.id),
            guest.id,
            OrgRole::Member
        )
        .await
        .unwrap(),
        RoleOutcome::SeatLimitReached
    ));
    tx.commit().await.unwrap();
    assert_eq!(
        owner.id,
        sqlx::query_scalar::<_, Uuid>(
            "SELECT user_id FROM organization_members WHERE org_id=$1 AND role='owner'"
        )
        .bind(org_id)
        .fetch_one(&db.pool)
        .await
        .unwrap()
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn acceptance_converts_its_reservation_and_fails_after_operator_shrinks_limit() {
    let db = TestDb::new().await;
    let (org_id, _, owner) = org(&db.pool, "seat-accept", 2).await;
    let scope = invitation_scope(org_id, owner.id);
    let now = time::OffsetDateTime::now_utc();
    let first = invitation(
        "accepted-seat@example.test".to_owned(),
        owner.id,
        now,
        OrgRole::Member,
    );
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let CreateInvitationOutcome::Created(first_invite) =
        tx.invitation_create(&scope, &first).await.unwrap()
    else {
        panic!("available seat creates invitation");
    };
    tx.commit().await.unwrap();
    let accepted = accept(
        &db.pool,
        &first.token_hash,
        &AcceptBy::NewAccount {
            display_name: "Accepted Invitee".to_owned(),
            password_hash: "test-hash".to_owned(),
        },
        now + time::Duration::minutes(1),
    )
    .await
    .unwrap();
    assert!(
        matches!(accepted, AcceptOutcome::Accepted { invitation_id, .. } if invitation_id == first_invite.id)
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.status='active' AND m.role <> 'guest' AND u.disabled_at IS NULL"
        )
        .bind(org_id)
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        2,
        "acceptance converts its reservation to one active seat"
    );

    sqlx::query("UPDATE organizations SET seat_limit=3 WHERE id=$1")
        .bind(org_id)
        .execute(&db.pool)
        .await
        .unwrap();
    let second = invitation(
        "over-limit-seat@example.test".to_owned(),
        owner.id,
        now + time::Duration::minutes(2),
        OrgRole::Member,
    );
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let CreateInvitationOutcome::Created(invite) =
        tx.invitation_create(&scope, &second).await.unwrap()
    else {
        panic!("available seat creates invitation");
    };
    tx.commit().await.unwrap();

    sqlx::query("UPDATE organizations SET seat_limit=1 WHERE id=$1")
        .bind(org_id)
        .execute(&db.pool)
        .await
        .unwrap();
    let outcome = accept(
        &db.pool,
        &second.token_hash,
        &AcceptBy::NewAccount {
            display_name: "Invitee".to_owned(),
            password_hash: "test-hash".to_owned(),
        },
        now + time::Duration::minutes(3),
    )
    .await
    .unwrap();
    assert_eq!(outcome, AcceptOutcome::SeatLimitReached);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM invitations WHERE id=$1 AND accepted_at IS NULL"
        )
        .bind(invite.id)
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM users WHERE normalized_email='over-limit-seat@example.test'"
        )
        .fetch_one(&db.pool)
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn revoked_and_expired_invites_release_reserved_seats() {
    let db = TestDb::new().await;
    let (org_id, _, owner) = org(&db.pool, "seat-release", 3).await;
    let invite_scope = invitation_scope(org_id, owner.id);
    let add_scope = member_add_scope(org_id);
    let now = time::OffsetDateTime::now_utc();

    let revoked = invitation(
        "revoked-seat@example.test".to_owned(),
        owner.id,
        now,
        OrgRole::Member,
    );
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let CreateInvitationOutcome::Created(revoked_invite) =
        tx.invitation_create(&invite_scope, &revoked).await.unwrap()
    else {
        panic!("available seat creates invitation");
    };
    tx.commit().await.unwrap();
    sqlx::query("UPDATE invitations SET revoked_at=$2 WHERE id=$1")
        .bind(revoked_invite.id)
        .bind(now + time::Duration::minutes(1))
        .execute(&db.pool)
        .await
        .unwrap();
    let first = user(&db.pool, "after-revoke@example.test").await;
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.add(&add_scope, first.id, OrgRole::Member).await.unwrap(),
        AddOutcome::Added(_)
    ));
    tx.commit().await.unwrap();

    let expired = invitation(
        "expired-seat@example.test".to_owned(),
        owner.id,
        now,
        OrgRole::Member,
    );
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    let CreateInvitationOutcome::Created(expired_invite) =
        tx.invitation_create(&invite_scope, &expired).await.unwrap()
    else {
        panic!("one seat remains for second invitation");
    };
    tx.commit().await.unwrap();
    sqlx::query(
        "UPDATE invitations SET created_at=$2 - INTERVAL '1 minute', expires_at=$2 WHERE id=$1",
    )
    .bind(expired_invite.id)
    .bind(now - time::Duration::minutes(1))
    .execute(&db.pool)
    .await
    .unwrap();
    let second = user(&db.pool, "after-expiry@example.test").await;
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.add(&add_scope, second.id, OrgRole::Member)
            .await
            .unwrap(),
        AddOutcome::Added(_)
    ));
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn account_reactivation_checks_every_active_membership_organization() {
    let db = TestDb::new().await;
    let (first_org, _, first_owner) = org(&db.pool, "seat-reactivate-a", 2).await;
    let (second_org, _, second_owner) = org(&db.pool, "seat-reactivate-b", 2).await;
    let shared = user(&db.pool, "shared-seat@example.test").await;
    for (org_id, owner) in [(first_org, &first_owner), (second_org, &second_owner)] {
        organizations::add_member(
            &db.pool,
            &member_add_scope(org_id),
            shared.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        assert!(owner.id != shared.id);
    }
    let now = time::OffsetDateTime::now_utc();
    assert!(matches!(
        accounts::set_disabled(&db.pool, shared.id, true, now)
            .await
            .unwrap(),
        DisableOutcome::Changed { .. }
    ));
    for (org_id, email) in [
        (first_org, "first-org-fill@example.test"),
        (second_org, "second-org-fill@example.test"),
    ] {
        let fill = user(&db.pool, email).await;
        let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
        assert!(matches!(
            tx.add(&member_add_scope(org_id), fill.id, OrgRole::Member)
                .await
                .unwrap(),
            AddOutcome::Added(_)
        ));
        tx.commit().await.unwrap();
    }
    let reenabled = accounts::set_disabled(&db.pool, shared.id, false, now)
        .await
        .unwrap();
    let DisableOutcome::SeatLimitReached { organizations } = reenabled else {
        panic!("full membership organizations block reactivation");
    };
    let mut expected = vec![first_org, second_org];
    expected.sort_unstable();
    assert_eq!(organizations, expected);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT disabled_at IS NOT NULL FROM users WHERE id=$1")
            .bind(shared.id)
            .fetch_one(&db.pool)
            .await
            .unwrap(),
        "failed reactivation leaves account disabled"
    );
    sqlx::query("UPDATE organizations SET seat_limit=3 WHERE id=ANY($1)")
        .bind([first_org, second_org].as_slice())
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        accounts::set_disabled(&db.pool, shared.id, false, now)
            .await
            .unwrap(),
        DisableOutcome::Changed { .. }
    ));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn concurrent_reactivation_and_member_add_never_exceed_seat_limit() {
    let db = TestDb::new().await;
    let (org_id, _, _) = org(&db.pool, "seat-reactivate-race", 2).await;
    let scope = member_add_scope(org_id);
    let disabled_member = user(&db.pool, "disabled-race-seat@example.test").await;
    let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
    assert!(matches!(
        tx.add(&scope, disabled_member.id, OrgRole::Member)
            .await
            .unwrap(),
        AddOutcome::Added(_)
    ));
    tx.commit().await.unwrap();
    let now = time::OffsetDateTime::now_utc();
    assert!(matches!(
        accounts::set_disabled(&db.pool, disabled_member.id, true, now)
            .await
            .unwrap(),
        DisableOutcome::Changed { .. }
    ));
    let candidate = user(&db.pool, "concurrent-candidate@example.test").await;

    let reenable_pool = db.pool.clone();
    let reenable = tokio::spawn(async move {
        accounts::set_disabled(&reenable_pool, disabled_member.id, false, now)
            .await
            .unwrap()
    });
    let add_pool = db.pool.clone();
    let add = tokio::spawn(async move {
        let mut tx = OrgTx::begin(&add_pool, org_id).await.unwrap().unwrap();
        let outcome = tx.add(&scope, candidate.id, OrgRole::Member).await.unwrap();
        tx.commit().await.unwrap();
        outcome
    });
    let (reenable, add) = tokio::join!(reenable, add);
    let reenable = reenable.unwrap();
    let add = add.unwrap();
    assert!(matches!(
        &reenable,
        DisableOutcome::Changed { .. } | DisableOutcome::SeatLimitReached { .. }
    ));
    assert!(matches!(
        &add,
        AddOutcome::Added(_) | AddOutcome::SeatLimitReached
    ));
    let active_seats: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM organization_members m JOIN users u ON u.id=m.user_id WHERE m.org_id=$1 AND m.status='active' AND m.role <> 'guest' AND u.disabled_at IS NULL",
    )
    .bind(org_id)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(
        active_seats <= 2,
        "active seats {active_seats} exceed limit 2"
    );
    assert!(matches!(
        (reenable, add),
        (DisableOutcome::Changed { .. }, AddOutcome::SeatLimitReached)
            | (
                DisableOutcome::SeatLimitReached { .. },
                AddOutcome::Added(_)
            )
    ));
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn reactivation_retries_when_membership_org_appears_during_lock_wait() {
    let db = TestDb::new().await;
    let (old_org, _, _) = org(&db.pool, "seat-snapshot-old", 4).await;
    let account = user(&db.pool, "snapshot-seat@example.test").await;
    let mut tx = OrgTx::begin(&db.pool, old_org).await.unwrap().unwrap();
    assert!(matches!(
        tx.add(&member_add_scope(old_org), account.id, OrgRole::Member)
            .await
            .unwrap(),
        AddOutcome::Added(_)
    ));
    tx.commit().await.unwrap();

    // Hold the old membership organization so enable snapshots its membership
    // set, then waits before it can lock the user row.
    let gate = OrgTx::begin(&db.pool, old_org).await.unwrap().unwrap();
    let enable_pool = db.pool.clone();
    let now = time::OffsetDateTime::now_utc();
    let enable = tokio::spawn(async move {
        accounts::set_disabled(&enable_pool, account.id, false, now)
            .await
            .unwrap()
    });
    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut waiting_on_old_org = false;
    while !waiting_on_old_org && tokio::time::Instant::now() < wait_deadline {
        waiting_on_old_org = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() \
             AND wait_event_type='Lock' AND query LIKE '%SELECT id FROM organizations WHERE id = ANY%')",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if !waiting_on_old_org {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(waiting_on_old_org, "enable waits for old organization lock");

    // New organization joins snapshot after enable started. Fill its last seat
    // in an uncommitted admission; retry must wait for this lock, then reject.
    let (new_org, _, _) = org(&db.pool, "seat-snapshot-new", 2).await;
    let mut add_account = OrgTx::begin(&db.pool, new_org).await.unwrap().unwrap();
    assert!(matches!(
        add_account
            .add(&member_add_scope(new_org), account.id, OrgRole::Member)
            .await
            .unwrap(),
        AddOutcome::Added(_)
    ));
    add_account.commit().await.unwrap();
    assert!(matches!(
        accounts::set_disabled(&db.pool, account.id, true, now)
            .await
            .unwrap(),
        DisableOutcome::Changed { .. }
    ));
    let candidate = user(&db.pool, "snapshot-seat-fill@example.test").await;
    let mut fill = OrgTx::begin(&db.pool, new_org).await.unwrap().unwrap();
    assert!(matches!(
        fill.add(&member_add_scope(new_org), candidate.id, OrgRole::Member)
            .await
            .unwrap(),
        AddOutcome::Added(_)
    ));
    gate.commit().await.unwrap();

    let wait_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut waiting_on_retry_lock = false;
    while !enable.is_finished()
        && !waiting_on_retry_lock
        && tokio::time::Instant::now() < wait_deadline
    {
        waiting_on_retry_lock = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() \
             AND wait_event_type='Lock' AND query LIKE '%SELECT id FROM organizations WHERE id = ANY%')",
        )
        .fetch_one(&db.pool)
        .await
        .unwrap();
        if !waiting_on_retry_lock {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    assert!(
        waiting_on_retry_lock,
        "retry waits for the newly discovered organization lock"
    );
    fill.commit().await.unwrap();
    let outcome = enable.await.unwrap();
    assert_eq!(
        outcome,
        DisableOutcome::SeatLimitReached {
            organizations: vec![new_org],
        },
        "revalidation and sorted retry include the new organization"
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT disabled_at IS NOT NULL FROM users WHERE id=$1")
            .bind(account.id)
            .fetch_one(&db.pool)
            .await
            .unwrap()
    );
}
