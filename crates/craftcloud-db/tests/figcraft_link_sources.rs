//! FigCraft capability-origin regressions against PostgreSQL.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{str::FromStr, time::Duration};

use craftcloud_core::policy::{
    Action, Actor, MemberTarget, ObjectTarget, OrgMembership, Resource, can,
};
use craftcloud_db::{
    OrgScope, PgPool, app_access,
    audit::AuditCtx,
    figcraft::{self, NewFigcraftLink},
    migrate,
    organizations::{self, MemberStatus, NewOrganization, OrgRole},
    users::{self, NewUser, User},
    workspaces::{self, NewWorkspace},
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;
use uuid::Uuid;

struct TestDb {
    name: String,
    admin: PgConnectOptions,
    pool: PgPool,
}

impl TestDb {
    async fn new() -> Self {
        let admin = PgConnectOptions::from_str(
            &std::env::var("CC_TEST_DATABASE_URL").expect("CC_TEST_DATABASE_URL must be set"),
        )
        .unwrap();
        let name = format!("cc_test_{}", Uuid::new_v4().simple());
        let mut connection = PgConnection::connect_with(&admin).await.unwrap();
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut connection)
            .await
            .unwrap();
        connection.close().await.unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(8)
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
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async {
                if let Ok(mut connection) = PgConnection::connect_with(&admin).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut connection)
                            .await;
                    let _ = connection.close().await;
                }
            });
        })
        .join();
    }
}

fn permitted_scope(org_id: Uuid, actor_id: Uuid, action: Action, resource: Resource) -> OrgScope {
    let actor = Actor {
        user_id: actor_id,
        instance_operator: false,
        membership: Some(OrgMembership {
            org_id,
            role: OrgRole::Owner,
        }),
        workspace: None,
    };
    let permit = can(&actor, action, &resource).permit().unwrap();
    OrgScope::from_permit(&permit).unwrap()
}

async fn create_user(pool: &PgPool, email: &str) -> User {
    users::insert(
        pool,
        &NewUser {
            normalized_email: email.to_owned(),
            display_name: email.to_owned(),
        },
    )
    .await
    .unwrap()
}

#[allow(clippy::too_many_arguments)] // Explicit fixture fields distinguish each link origin.
async fn insert_link(
    pool: &PgPool,
    scope: &OrgScope,
    workspace_id: Uuid,
    file_id: Uuid,
    user_id: Uuid,
    source_kind: &str,
    source_app: Option<&str>,
    access: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let remote_link = format!("remote-{}", Uuid::new_v4());
    let mut tx = craftcloud_db::members::OrgTx::begin(pool, scope.org_id())
        .await
        .unwrap()
        .unwrap();
    assert!(
        tx.figcraft_insert_link(
            scope,
            NewFigcraftLink {
                id,
                file_id,
                user_id,
                access,
                remote_file: "remote-file",
                remote_link: &remote_link,
                ciphertext: &[9, 8, 7],
                now: OffsetDateTime::now_utc(),
                workspace_id,
                source_kind,
                source_app,
            },
            &AuditCtx::system()
        )
        .await
        .unwrap()
    );
    tx.commit().await.unwrap();
    id
}

async fn active_rows(
    pool: &PgPool,
    file_id: Uuid,
    user_id: Uuid,
) -> Vec<(String, Option<String>, String)> {
    sqlx::query_as("SELECT source_kind,source_app,access FROM figcraft_links WHERE file_id=$1 AND user_id=$2 AND revoked_at IS NULL ORDER BY source_kind,source_app,access")
        .bind(file_id).bind(user_id).fetch_all(pool).await.unwrap()
}

async fn finish_outbox(pool: &PgPool) -> Vec<String> {
    let now = OffsetDateTime::now_utc() + time::Duration::seconds(1);
    let rows = figcraft::claim_revocations(pool, 100, now).await.unwrap();
    let reasons = rows.iter().map(|row| row.reason.clone()).collect();
    for row in rows {
        figcraft::finish_revocation(pool, row.id, now)
            .await
            .unwrap();
    }
    reasons
}

async fn make_live_figcraft_file(
    pool: &PgPool,
    org_id: Uuid,
    owner_id: Uuid,
    workspace_id: Uuid,
) -> Uuid {
    let now = OffsetDateTime::now_utc();
    let project_id = Uuid::new_v4();
    sqlx::query("INSERT INTO projects(id,org_id,workspace_id,name,name_key,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,'Source project','source-project',$4,$5,$4,$5)")
        .bind(project_id).bind(org_id).bind(workspace_id).bind(owner_id).bind(now).execute(pool).await.unwrap();
    let file_id = Uuid::new_v4();
    sqlx::query("INSERT INTO files(id,org_id,workspace_id,project_id,name,name_key,app,format,created_by_user_id,created_at,updated_by_user_id,updated_at) VALUES($1,$2,$3,$4,'Source file','source-file','figcraft','fig',$5,$6,$5,$6)")
        .bind(file_id).bind(org_id).bind(workspace_id).bind(project_id).bind(owner_id).bind(now).execute(pool).await.unwrap();
    let blob_id = Uuid::new_v4();
    sqlx::query("INSERT INTO blobs(id,org_id,workspace_id,object_key,size_bytes,sha256,state,created_at) VALUES($1,$2,$3,$4,0,$5,'committed',$6)")
        .bind(blob_id).bind(org_id).bind(workspace_id).bind(Uuid::new_v4().simple().to_string())
        .bind([0_u8; 32].as_slice()).bind(now).execute(pool).await.unwrap();
    let version_id = Uuid::new_v4();
    sqlx::query("INSERT INTO file_versions(id,org_id,workspace_id,project_id,file_id,number,blob_id,size_bytes,sha256,created_by_user_id,created_at) VALUES($1,$2,$3,$4,$5,1,$6,0,$7,$8,$9)")
        .bind(version_id).bind(org_id).bind(workspace_id).bind(project_id).bind(file_id).bind(blob_id)
        .bind([0_u8; 32].as_slice()).bind(owner_id).bind(now).execute(pool).await.unwrap();
    sqlx::query("UPDATE files SET head_version_id=$2 WHERE id=$1")
        .bind(file_id)
        .bind(version_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO figcraft_live_files(file_id,org_id,workspace_id,figcraft_file_id,published_from_version_id,state,published_by_user_id,created_at,updated_at) VALUES($1,$2,$3,'remote-file',$4,'live',$5,$6,$6)")
        .bind(file_id).bind(org_id).bind(workspace_id).bind(version_id).bind(owner_id).bind(now).execute(pool).await.unwrap();
    file_id
}

#[tokio::test]
#[ignore = "requires PostgreSQL: cargo xtask db-test"]
async fn source_specific_app_revocation_preserves_other_links_and_generic_revokes_cover_all_sources()
 {
    let db = TestDb::new().await;
    let pool = &db.pool;
    let owner = create_user(pool, "source-owner@example.test").await;
    let member = create_user(pool, "source-member@example.test").await;
    let org = organizations::create(
        pool,
        &NewOrganization {
            slug: format!("source-{}", Uuid::new_v4().simple()),
            name: "Link sources".to_owned(),
            owner_user_id: owner.id,
        },
    )
    .await
    .unwrap();
    let org_id = org.id;
    let add_owner = permitted_scope(
        org_id,
        owner.id,
        Action::MemberAdd,
        Resource::Member {
            org_id,
            target: None,
            role: Some(OrgRole::Owner),
        },
    );
    organizations::add_member(
        pool,
        &add_owner,
        owner.id,
        OrgRole::Owner,
        MemberStatus::Active,
    )
    .await
    .unwrap();
    let add_member = permitted_scope(
        org_id,
        owner.id,
        Action::MemberAdd,
        Resource::Member {
            org_id,
            target: None,
            role: Some(OrgRole::Member),
        },
    );
    organizations::add_member(
        pool,
        &add_member,
        member.id,
        OrgRole::Member,
        MemberStatus::Active,
    )
    .await
    .unwrap();
    let workspace = workspaces::create(
        pool,
        &permitted_scope(
            org_id,
            owner.id,
            Action::WorkspaceCreate,
            Resource::Organization { org_id },
        ),
        &NewWorkspace {
            slug: "source-ws".to_owned(),
            name: "Source workspace".to_owned(),
            quota_bytes: None,
        },
    )
    .await
    .unwrap();
    let file_id = make_live_figcraft_file(pool, org_id, owner.id, workspace.id).await;
    let file_scope = permitted_scope(
        org_id,
        owner.id,
        Action::FileRead,
        Resource::Object {
            org_id,
            workspace_id: workspace.id,
            target: ObjectTarget::File { grant_role: None },
        },
    );

    let sources = [
        ("browser", None),
        ("launcher", None),
        ("app", Some("photocraft")),
        ("app", Some("vectorcraft")),
        ("app", Some("legacy")),
        ("legacy", None),
    ];
    let mut issued = Vec::new();
    for (kind, app) in sources {
        for access in ["view", "edit"] {
            let id = insert_link(
                pool,
                &file_scope,
                workspace.id,
                file_id,
                member.id,
                kind,
                app,
                access,
            )
            .await;
            issued.push((
                kind.to_owned(),
                app.map(str::to_owned),
                access.to_owned(),
                id,
            ));
        }
    }
    for (kind, app, access, expected_id) in &issued {
        let reused = figcraft::live_link(pool, file_id, member.id, access, kind, app.as_deref())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reused.id, *expected_id,
            "same link origin must reuse its capability"
        );
    }
    assert!(
        figcraft::live_link(pool, file_id, member.id, "view", "app", Some("figcraft"))
            .await
            .unwrap()
            .is_none(),
        "different app identity must not reuse another app link"
    );

    let helper_time = OffsetDateTime::now_utc();
    figcraft::enqueue_file_app_revocations_pool(
        pool,
        org_id,
        file_id,
        member.id,
        Some("photocraft"),
        helper_time,
    )
    .await
    .unwrap();
    let helper_outbox =
        figcraft::claim_revocations(pool, 100, helper_time + time::Duration::seconds(1))
            .await
            .unwrap();
    assert_eq!(helper_outbox.len(), 1);
    assert_eq!(helper_outbox[0].reason, "app_access_revoked:app:photocraft");
    let mut queued_access: Vec<String> =
        figcraft::app_access_revocation_links(pool, file_id, member.id, Some("photocraft"))
            .await
            .unwrap()
            .into_iter()
            .map(|link| link.access)
            .collect();
    queued_access.sort();
    assert_eq!(
        queued_access,
        ["edit", "view"],
        "one file/app outbox row covers both link access levels"
    );
    figcraft::finish_revocation(
        pool,
        helper_outbox[0].id,
        helper_time + time::Duration::seconds(1),
    )
    .await
    .unwrap();
    assert!(
        !active_rows(pool, file_id, member.id)
            .await
            .iter()
            .any(|(kind, app, _)| kind == "app" && app.as_deref() == Some("photocraft"))
    );
    for access in ["view", "edit"] {
        insert_link(
            pool,
            &file_scope,
            workspace.id,
            file_id,
            member.id,
            "app",
            Some("photocraft"),
            access,
        )
        .await;
    }

    sqlx::query("INSERT INTO member_app_access(org_id,user_id,app) VALUES($1,$2,'photocraft'),($1,$2,'vectorcraft'),($1,$2,'legacy')")
        .bind(org_id).bind(member.id).execute(pool).await.unwrap();
    let assignment_scope = permitted_scope(
        org_id,
        owner.id,
        Action::MemberAppsSet,
        Resource::Member {
            org_id,
            target: Some(MemberTarget {
                user_id: member.id,
                role: OrgRole::Member,
            }),
            role: None,
        },
    );
    app_access::set_assignments(
        pool,
        &assignment_scope,
        member.id,
        &["vectorcraft".to_owned(), "legacy".to_owned()],
        &AuditCtx::system(),
    )
    .await
    .unwrap();
    let revoke_time = OffsetDateTime::now_utc() + time::Duration::seconds(1);
    let pending = figcraft::claim_revocations(pool, 100, revoke_time)
        .await
        .unwrap();
    let reasons: Vec<String> = pending.iter().map(|row| row.reason.clone()).collect();
    assert!(
        reasons.contains(&"app_access_revoked:app:photocraft".to_owned()),
        "{reasons:?}"
    );
    assert!(
        reasons.contains(&"app_access_revoked:legacy".to_owned()),
        "{reasons:?}"
    );
    for source in [Some("photocraft"), None, Some("legacy")] {
        let mut access: Vec<String> =
            figcraft::app_access_revocation_links(pool, file_id, member.id, source)
                .await
                .unwrap()
                .into_iter()
                .map(|link| link.access)
                .collect();
        access.sort();
        assert_eq!(access, ["edit", "view"]);
    }
    assert_eq!(
        figcraft::app_access_revocation_links(pool, file_id, member.id, Some("legacy"))
            .await
            .unwrap()
            .len(),
        2,
        "the app id `legacy` is distinct from the unknown-origin legacy sentinel"
    );
    for row in pending {
        figcraft::finish_revocation(pool, row.id, revoke_time)
            .await
            .unwrap();
    }
    let still_active = active_rows(pool, file_id, member.id).await;
    assert_eq!(still_active.len(), 8);
    for (kind, app, access) in &still_active {
        assert!(
            !(*kind == "app" && app.as_deref() == Some("photocraft")) && kind != "legacy",
            "only photocraft and unknown-origin legacy links should be revoked; kept {kind}/{app:?}/{access}"
        );
    }
    for expected in [
        ("browser", None, "view"),
        ("browser", None, "edit"),
        ("launcher", None, "view"),
        ("launcher", None, "edit"),
        ("app", Some("vectorcraft"), "view"),
        ("app", Some("vectorcraft"), "edit"),
    ] {
        assert!(
            still_active
                .iter()
                .any(|(kind, app, access)| kind == expected.0
                    && app.as_deref() == expected.1
                    && access == expected.2)
        );
    }
    for access in ["view", "edit"] {
        assert!(
            still_active
                .iter()
                .any(|(kind, app, found_access)| kind == "app"
                    && app.as_deref() == Some("legacy")
                    && found_access == access)
        );
    }

    let mut grant_tx = pool.begin().await.unwrap();
    figcraft::enqueue_file_user_revocations(
        &mut grant_tx,
        file_id,
        member.id,
        "grant_revoked",
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    grant_tx.commit().await.unwrap();
    assert!(
        finish_outbox(pool)
            .await
            .contains(&"grant_revoked".to_owned())
    );
    assert!(
        active_rows(pool, file_id, member.id).await.is_empty(),
        "generic grant revocation covers all origins"
    );

    for (kind, app) in sources {
        for access in ["view", "edit"] {
            insert_link(
                pool,
                &file_scope,
                workspace.id,
                file_id,
                member.id,
                kind,
                app,
                access,
            )
            .await;
        }
    }
    let mut member_tx = pool.begin().await.unwrap();
    figcraft::enqueue_user_revocations(
        &mut member_tx,
        org_id,
        member.id,
        "membership_removed",
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    member_tx.commit().await.unwrap();
    assert!(
        finish_outbox(pool)
            .await
            .contains(&"membership_removed".to_owned())
    );
    assert!(
        active_rows(pool, file_id, member.id).await.is_empty(),
        "generic membership revocation covers all origins"
    );
}
