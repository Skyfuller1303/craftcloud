//! Integration tests against a real PostgreSQL server.
//!
//! Run with `cargo xtask db-test`, which starts a throwaway container and sets
//! `CC_TEST_DATABASE_URL`. Every test creates its own database
//! `cc_test_<uuid>` and drops it afterwards.

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use craftcloud_db::migrate::{self, migrator_up_to};
    use craftcloud_db::organizations::{self, MemberStatus, NewOrganization, OrgRole};
    use craftcloud_db::sessions::{self, NewSession};
    use craftcloud_db::setup::{self, NewInstance, SetupError};
    use craftcloud_db::users::{self, NewUser, User};
    use craftcloud_db::workspaces::{self, MAX_LIST_LIMIT, NewWorkspace, WorkspaceRole};
    use craftcloud_db::{DbError, OrgScope, PgPool, SchemaProblem};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection, Row};
    use uuid::Uuid;

    const ENV: &str = "CC_TEST_DATABASE_URL";

    /// A throwaway database, dropped (with FORCE) when the guard is dropped.
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
            let pool = Self::pool_for(&admin, &name, 8).await;
            Self { name, admin, pool }
        }

        async fn pool_for(admin: &PgConnectOptions, name: &str, max: u32) -> PgPool {
            PgPoolOptions::new()
                .max_connections(max)
                .acquire_timeout(Duration::from_secs(10))
                .connect_with(admin.clone().database(name))
                .await
                .unwrap()
        }

        async fn extra_pool(&self) -> PgPool {
            Self::pool_for(&self.admin, &self.name, 4).await
        }
    }

    impl Drop for TestDb {
        fn drop(&mut self) {
            let admin = self.admin.clone();
            let name = self.name.clone();
            // Drop runs inside a runtime; use a separate thread with its own.
            let _ = std::thread::spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async {
                    if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        ))
                        .execute(&mut conn)
                        .await;
                        let _ = conn.close().await;
                    }
                });
            })
            .join();
        }
    }

    async fn migrated() -> TestDb {
        let db = TestDb::new().await;
        migrate::run(&db.pool).await.unwrap();
        db
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

    /// Creates an organization owned by a fresh user who is also its owner member.
    async fn org(pool: &PgPool, slug: &str) -> (OrgScope, User) {
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
        let scope = scope_for(created.id);
        organizations::add_member(
            pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            owner.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        (scope, owner)
    }

    /// A scope can only come from a policy `Allow`; tests ask the policy as an
    /// owner of the organization.
    fn scope_for(org_id: Uuid) -> OrgScope {
        use craftcloud_core::policy::{Action, Actor, OrgMembership, Resource, can};
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

    /// The scope for exactly one action (every writing repository function
    /// requires the scope of its own action).
    fn sc(scope: &OrgScope, action: craftcloud_core::policy::Action) -> OrgScope {
        use craftcloud_core::policy::{Actor, OrgMembership, Resource, can};
        let org_id = scope.org_id();
        let actor = Actor {
            user_id: Uuid::new_v4(),
            instance_operator: false,
            membership: Some(OrgMembership {
                org_id,
                role: OrgRole::Owner,
            }),
            workspace: None,
        };
        let resource = match action {
            craftcloud_core::policy::Action::MemberAdd => Resource::Member {
                org_id,
                target: None,
                role: Some(OrgRole::Member),
            },
            craftcloud_core::policy::Action::MemberChangeRole
            | craftcloud_core::policy::Action::MemberRemove => Resource::Member {
                org_id,
                target: Some(craftcloud_core::policy::MemberTarget {
                    user_id: Uuid::new_v4(),
                    role: OrgRole::Member,
                }),
                role: Some(OrgRole::Admin),
            },
            craftcloud_core::policy::Action::WorkspaceMemberAdd
            | craftcloud_core::policy::Action::WorkspaceMemberChangeRole
            | craftcloud_core::policy::Action::WorkspaceMemberRemove
            | craftcloud_core::policy::Action::WorkspaceMemberList => Resource::WorkspaceMember {
                org_id,
                workspace_id: Uuid::new_v4(),
                target: Some(craftcloud_core::policy::WorkspaceMemberTarget {
                    user_id: Uuid::new_v4(),
                    role: craftcloud_core::policy::WorkspaceRole::Viewer,
                }),
                role: Some(craftcloud_core::policy::WorkspaceRole::Viewer),
            },
            craftcloud_core::policy::Action::WorkspaceManage
            | craftcloud_core::policy::Action::WorkspaceRead => Resource::Workspace {
                org_id,
                workspace_id: Uuid::new_v4(),
            },
            _ => Resource::Organization { org_id },
        };
        let permit = can(&actor, action, &resource).permit().unwrap();
        OrgScope::from_permit(&permit).unwrap()
    }

    /// A scope stemming from a read-only action.
    fn read_scope_for(org_id: Uuid) -> OrgScope {
        use craftcloud_core::policy::{Action, Actor, OrgMembership, Resource, can};
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
            Action::MemberList,
            &Resource::Member {
                org_id,
                target: None,
                role: None,
            },
        )
        .permit()
        .unwrap();
        OrgScope::from_permit(&permit).unwrap()
    }

    /// Like `org`, but for databases migrated only to an older schema (before
    /// migration 0005 added `organizations.version`).
    async fn legacy_org(pool: &PgPool, slug: &str) -> (OrgScope, User) {
        let owner = user(pool, &format!("owner-{slug}@example.test")).await;
        let org_id = Uuid::new_v4();
        sqlx::query("INSERT INTO organizations (id, slug, name, owner_user_id) VALUES ($1, $2, 'Legacy', $3)")
            .bind(org_id)
            .bind(slug)
            .bind(owner.id)
            .execute(pool)
            .await
            .unwrap();
        let scope = scope_for(org_id);
        organizations::add_member(
            pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            owner.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        (scope, owner)
    }

    fn new_ws(slug: &str) -> NewWorkspace {
        NewWorkspace {
            slug: slug.to_owned(),
            name: format!("Workspace {slug}"),
            quota_bytes: None,
        }
    }

    fn db_err(error: sqlx::Error) -> DbError {
        DbError::from(error)
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn fresh_database_migrates_and_verifies() {
        let db = TestDb::new().await;
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::NotMigrated))
        );
        let applied = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            applied.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        migrate::verify(&db.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn migrating_twice_is_idempotent() {
        let db = TestDb::new().await;
        migrate::run(&db.pool).await.unwrap();
        assert!(migrate::run(&db.pool).await.unwrap().is_empty());
        migrate::verify(&db.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_migrators_apply_once() {
        let db = TestDb::new().await;
        let other = db.extra_pool().await;
        let (a, b) = tokio::join!(migrate::run(&db.pool), migrate::run(&other));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(
            a.len() + b.len(),
            12,
            "each migration applied exactly once in total"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 12);
        migrate::verify(&db.pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upgrade_keeps_existing_data() {
        let db = TestDb::new().await;
        let first = migrate::run_with(&db.pool, &migrator_up_to(1))
            .await
            .unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::Pending(vec![
                2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12
            ])))
        );

        let (scope, owner) = legacy_org(&db.pool, "acme").await;

        let second = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            second.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        migrate::verify(&db.pool).await.unwrap();

        let row = sqlx::query(
            "SELECT u.normalized_email, o.slug, m.role FROM users u \
             JOIN organizations o ON o.owner_user_id = u.id \
             JOIN organization_members m ON m.org_id = o.id AND m.user_id = u.id \
             WHERE o.id = $1",
        )
        .bind(scope.org_id())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            row.get::<String, _>("normalized_email"),
            owner.normalized_email
        );
        assert_eq!(row.get::<String, _>("slug"), "acme");
        assert_eq!(row.get::<String, _>("role"), "owner");
        // New tables are usable after the upgrade.
        workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn tampered_checksum_is_detected() {
        let db = migrated().await;
        sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00'::bytea WHERE version = 1")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::ChecksumMismatch(1)))
        );
        assert!(matches!(
            migrate::run(&db.pool).await,
            Err(DbError::Migration(_))
        ));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn unknown_and_failed_migrations_are_detected() {
        let db = migrated().await;
        sqlx::query(
            "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
             VALUES (99, 'future', true, '\\x00'::bytea, 0)",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::UnknownApplied(99)))
        );
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 99")
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET success = false WHERE version = 2")
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            migrate::verify(&db.pool).await,
            Err(DbError::Schema(SchemaProblem::Dirty(2)))
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspace_member_requires_org_membership() {
        let db = migrated().await;
        let (scope, _) = org(&db.pool, "acme").await;
        let ws = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        let outsider = user(&db.pool, "outsider@example.test").await;
        let err = workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            outsider.id,
            WorkspaceRole::Viewer,
        )
        .await
        .unwrap_err();
        assert_eq!(err.constraint(), Some("workspace_members_org_member_fk"));
        assert!(matches!(err, DbError::ForeignKeyViolation { .. }));

        // Becomes possible once the user is an organization member.
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            outsider.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            outsider.id,
            WorkspaceRole::Viewer,
        )
        .await
        .unwrap();
        let again = workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            outsider.id,
            WorkspaceRole::Editor,
        )
        .await
        .unwrap_err();
        assert!(matches!(again, DbError::UniqueViolation { .. }));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn workspace_member_cannot_cross_organizations() {
        let db = migrated().await;
        let (scope_a, _) = org(&db.pool, "org-a").await;
        let (scope_b, owner_b) = org(&db.pool, "org-b").await;
        let ws_a = workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        // owner_b is an org-b member, but ws_a belongs to org-a.
        let err = workspaces::add_member(
            &db.pool,
            &sc(
                &scope_b,
                craftcloud_core::policy::Action::WorkspaceMemberAdd,
            ),
            ws_a.id,
            owner_b.id,
            WorkspaceRole::Admin,
        )
        .await
        .unwrap_err();
        assert_eq!(err.constraint(), Some("workspace_members_workspace_fk"));
        // Raw attempt to attach an org-a workspace row to org-b is rejected too.
        let raw = sqlx::query(
            "INSERT INTO workspace_members (org_id, workspace_id, user_id, role) VALUES ($1, $2, $3, 'admin')",
        )
        .bind(scope_a.org_id())
        .bind(ws_a.id)
        .bind(owner_b.id)
        .execute(&db.pool)
        .await
        .unwrap_err();
        assert!(matches!(db_err(raw), DbError::ForeignKeyViolation { .. }));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn slug_is_unique_per_organization_only() {
        let db = migrated().await;
        let (scope_a, _) = org(&db.pool, "org-a").await;
        let (scope_b, _) = org(&db.pool, "org-b").await;
        workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        let dup = workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));
        assert_eq!(dup.constraint(), Some("workspaces_org_slug_unique"));
        workspaces::create(
            &db.pool,
            &sc(&scope_b, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn check_constraints_reject_bad_values() {
        let db = migrated().await;
        let bad_email = users::insert(
            &db.pool,
            &NewUser {
                normalized_email: "Upper@Example.test".to_owned(),
                display_name: "X".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(bad_email.constraint(), Some("users_email_normalized"));

        let bad_name = users::insert(
            &db.pool,
            &NewUser {
                normalized_email: "ok@example.test".to_owned(),
                display_name: String::new(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(bad_name, DbError::CheckViolation { .. }));

        user(&db.pool, "dup@example.test").await;
        let dup = users::insert(
            &db.pool,
            &NewUser {
                normalized_email: "dup@example.test".to_owned(),
                display_name: "Again".to_owned(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));

        let (scope, _) = org(&db.pool, "acme").await;
        let negative = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &NewWorkspace {
                quota_bytes: Some(-1),
                ..new_ws("neg")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(negative.constraint(), Some("workspaces_quota_non_negative"));
        workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &NewWorkspace {
                quota_bytes: Some(0),
                ..new_ws("zero")
            },
        )
        .await
        .unwrap();

        for bad_slug in ["Upper", "-lead", "has space", "", &"a".repeat(64)] {
            let err = workspaces::create(
                &db.pool,
                &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
                &new_ws(bad_slug),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, DbError::CheckViolation { .. }),
                "{bad_slug:?}"
            );
        }
        let bad_role = sqlx::query(
            "INSERT INTO organization_members (org_id, user_id, role, status) VALUES ($1, $2, 'god', 'active')",
        )
        .bind(scope.org_id())
        .bind(Uuid::new_v4())
        .execute(&db.pool)
        .await
        .unwrap_err();
        assert!(matches!(
            db_err(bad_role),
            DbError::CheckViolation { .. } | DbError::ForeignKeyViolation { .. }
        ));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn queries_never_cross_organizations() {
        let db = migrated().await;
        let (scope_a, _) = org(&db.pool, "org-a").await;
        let (scope_b, _) = org(&db.pool, "org-b").await;
        let ws_a = workspaces::create(
            &db.pool,
            &sc(&scope_a, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("a-one"),
        )
        .await
        .unwrap();
        let ws_b = workspaces::create(
            &db.pool,
            &sc(&scope_b, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("b-one"),
        )
        .await
        .unwrap();

        assert_eq!(
            workspaces::get(&db.pool, &scope_a, ws_a.id)
                .await
                .unwrap()
                .map(|w| w.id),
            Some(ws_a.id)
        );
        // Another organization's id looks exactly like a missing one.
        assert!(
            workspaces::get(&db.pool, &scope_a, ws_b.id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            workspaces::get(&db.pool, &scope_a, Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );

        let listed = workspaces::list(&db.pool, &scope_a, 50).await.unwrap();
        assert_eq!(
            listed.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![ws_a.id]
        );
        assert!(listed.iter().all(|w| w.org_id == scope_a.org_id()));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn list_enforces_hard_cap() {
        let db = migrated().await;
        let (scope, _) = org(&db.pool, "acme").await;
        sqlx::query(
            "INSERT INTO workspaces (id, org_id, slug, name) \
             SELECT gen_random_uuid(), $1, 'w' || g, 'W ' || g FROM generate_series(1, $2) g",
        )
        .bind(scope.org_id())
        .bind(i32::try_from(MAX_LIST_LIMIT).unwrap() + 5)
        .execute(&db.pool)
        .await
        .unwrap();
        let capped = workspaces::list(&db.pool, &scope, 10_000).await.unwrap();
        assert_eq!(capped.len(), MAX_LIST_LIMIT as usize);
        assert_eq!(
            workspaces::list(&db.pool, &scope, 0).await.unwrap().len(),
            1
        );
        assert_eq!(
            workspaces::list(&db.pool, &scope, 5).await.unwrap().len(),
            5
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn deletes_are_restricted() {
        let db = migrated().await;
        let (scope, owner) = org(&db.pool, "acme").await;
        let ws = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            owner.id,
            WorkspaceRole::Admin,
        )
        .await
        .unwrap();

        let e1 = sqlx::query("DELETE FROM workspaces WHERE id = $1")
            .bind(ws.id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        let e2 = sqlx::query("DELETE FROM organization_members WHERE org_id = $1")
            .bind(scope.org_id())
            .execute(&db.pool)
            .await
            .unwrap_err();
        let e3 = sqlx::query("DELETE FROM organizations WHERE id = $1")
            .bind(scope.org_id())
            .execute(&db.pool)
            .await
            .unwrap_err();
        let e4 = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(owner.id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        for e in [e1, e2, e3, e4] {
            assert!(matches!(db_err(e), DbError::ForeignKeyViolation { .. }));
        }
        assert!(
            workspaces::get(&db.pool, &scope, ws.id)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn connect_rejects_bad_urls_without_echo() {
        let err = craftcloud_db::connect("not a url", 1, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(err, DbError::InvalidUrl);
        let unreachable = "postgres://user:hunter2@127.0.0.1:1/db";
        let err = craftcloud_db::connect(unreachable, 1, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"));
        assert!(!format!("{err:?}").contains("hunter2"));
    }

    fn new_instance(slug: &str) -> NewInstance {
        NewInstance {
            operator_email: "operator@example.test".to_owned(),
            operator_display_name: "Operator".to_owned(),
            operator_password_hash: "$argon2id$placeholder".to_owned(),
            organization_name: "First Org".to_owned(),
            organization_slug: slug.to_owned(),
        }
    }

    async fn count(pool: &PgPool, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_creates_operator_org_and_membership_once() {
        let db = migrated().await;
        assert!(!setup::is_completed(&db.pool).await.unwrap());
        let created = setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap();
        assert!(setup::is_completed(&db.pool).await.unwrap());

        let row = sqlx::query("SELECT instance_role, password_hash FROM users WHERE id = $1")
            .bind(created.user_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            row.get::<Option<String>, _>("instance_role").as_deref(),
            Some("operator")
        );
        assert_eq!(
            row.get::<Option<String>, _>("password_hash").as_deref(),
            Some("$argon2id$placeholder")
        );
        let member = sqlx::query(
            "SELECT role, status FROM organization_members WHERE org_id = $1 AND user_id = $2",
        )
        .bind(created.organization_id)
        .bind(created.user_id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(member.get::<String, _>("role"), "owner");
        assert_eq!(member.get::<String, _>("status"), "active");
        let owner: Uuid =
            sqlx::query_scalar("SELECT owner_user_id FROM organizations WHERE id = $1")
                .bind(created.organization_id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(owner, created.user_id);
        let marker: Uuid = sqlx::query_scalar("SELECT operator_user_id FROM instance_setup")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(marker, created.user_id);

        let again = setup::complete(
            &db.pool,
            &NewInstance {
                operator_email: "other@example.test".to_owned(),
                ..new_instance("other")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(again, SetupError::AlreadyCompleted);
        assert_eq!(count(&db.pool, "users").await, 1);
        assert_eq!(count(&db.pool, "organizations").await, 1);
        assert_eq!(count(&db.pool, "instance_setup").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_failure_rolls_back_everything() {
        let db = migrated().await;
        // A pre-existing organization makes the slug insert fail after the
        // singleton row and the operator user were already written.
        org(&db.pool, "acme").await;
        let before_users = count(&db.pool, "users").await;

        let err = setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SetupError::Db(DbError::UniqueViolation { .. })),
            "{err:?}"
        );
        assert_eq!(count(&db.pool, "users").await, before_users);
        assert_eq!(count(&db.pool, "instance_setup").await, 0);
        assert!(!setup::is_completed(&db.pool).await.unwrap());
        let operators: i64 =
            sqlx::query_scalar("SELECT count(*) FROM users WHERE instance_role = 'operator'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(operators, 0);

        // The failed attempt did not close setup.
        setup::complete(&db.pool, &new_instance("fresh"))
            .await
            .unwrap();
        assert!(setup::is_completed(&db.pool).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_setups_produce_exactly_one_winner() {
        let db = migrated().await;
        let mut handles = Vec::new();
        for i in 0..10 {
            let pool = db.pool.clone();
            handles.push(tokio::spawn(async move {
                let new = NewInstance {
                    operator_email: format!("op{i}@example.test"),
                    ..new_instance(&format!("org-{i}"))
                };
                setup::complete(&pool, &new).await
            }));
        }
        let mut ok = 0;
        let mut already = 0;
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => ok += 1,
                Err(SetupError::AlreadyCompleted) => already += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!((ok, already), (1, 9));
        assert_eq!(count(&db.pool, "users").await, 1);
        assert_eq!(count(&db.pool, "organizations").await, 1);
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        assert_eq!(count(&db.pool, "instance_setup").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn upgrade_from_0002_keeps_data_and_leaves_setup_open() {
        let db = TestDb::new().await;
        migrate::run_with(&db.pool, &migrator_up_to(2))
            .await
            .unwrap();
        let (scope, _) = legacy_org(&db.pool, "acme").await;
        // Created with raw SQL: the repository functions target the current schema.
        let ws_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO workspaces (id, org_id, slug, name) VALUES ($1, $2, 'main', 'Main')",
        )
        .bind(ws_id)
        .bind(scope.org_id())
        .execute(&db.pool)
        .await
        .unwrap();

        let applied = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            applied.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );
        migrate::verify(&db.pool).await.unwrap();
        let ws = workspaces::get(&db.pool, &scope, ws_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ws.version, 1,
            "existing workspaces start at version 1 after migration 0006"
        );
        // Existing organizations start at version 1 (0005) with the invite setting on (0006).
        let upgraded = organizations::get(&db.pool, &scope).await.unwrap().unwrap();
        assert_eq!(upgraded.version, 1);
        assert!(upgraded.workspace_admin_invites_enabled);
        assert!(!setup::is_completed(&db.pool).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn instance_setup_is_a_singleton() {
        let db = migrated().await;
        let created = setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap();
        // A second row cannot be inserted, and id=false is rejected.
        let dup = sqlx::query("INSERT INTO instance_setup (operator_user_id) VALUES ($1)")
            .bind(created.user_id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        assert!(matches!(db_err(dup), DbError::UniqueViolation { .. }));
        let falsy =
            sqlx::query("INSERT INTO instance_setup (id, operator_user_id) VALUES (false, $1)")
                .bind(created.user_id)
                .execute(&db.pool)
                .await
                .unwrap_err();
        assert!(matches!(db_err(falsy), DbError::CheckViolation { .. }));
        // The operator cannot be deleted while referenced.
        let del = sqlx::query("DELETE FROM users WHERE id = $1")
            .bind(created.user_id)
            .execute(&db.pool)
            .await
            .unwrap_err();
        assert!(matches!(db_err(del), DbError::ForeignKeyViolation { .. }));
    }

    /// Documents the offline recovery path of docs/06-operations/deployment.md:
    /// deleting the marker row reopens setup, existing rows stay.
    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn deleting_the_marker_row_reopens_setup_without_touching_existing_data() {
        let db = migrated().await;
        setup::complete(&db.pool, &new_instance("acme"))
            .await
            .unwrap();
        sqlx::query("DELETE FROM instance_setup")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(!setup::is_completed(&db.pool).await.unwrap());

        // The old operator still owns its e-mail address and the old slug.
        let same_email = setup::complete(&db.pool, &new_instance("second"))
            .await
            .unwrap_err();
        assert!(matches!(
            same_email,
            SetupError::Db(DbError::UniqueViolation { .. })
        ));
        let fresh = NewInstance {
            operator_email: "second@example.test".to_owned(),
            ..new_instance("second")
        };
        setup::complete(&db.pool, &fresh).await.unwrap();
        assert_eq!(count(&db.pool, "users").await, 2);
        assert_eq!(count(&db.pool, "organizations").await, 2);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn setup_race_is_correct_under_a_serializable_server_default() {
        let db = migrated().await;
        // New sessions of this database default to SERIALIZABLE; the setup
        // transaction must still behave as under READ COMMITTED.
        let mut admin = PgConnection::connect_with(&db.admin).await.unwrap();
        sqlx::query(&format!(
            "ALTER DATABASE \"{}\" SET default_transaction_isolation = 'serializable'",
            db.name
        ))
        .execute(&mut admin)
        .await
        .unwrap();
        let fresh = db.extra_pool().await;
        let level: String = sqlx::query_scalar("SHOW default_transaction_isolation")
            .fetch_one(&fresh)
            .await
            .unwrap();
        assert_eq!(level, "serializable");

        let mut handles = Vec::new();
        for i in 0..10 {
            let pool = fresh.clone();
            handles.push(tokio::spawn(async move {
                let new = NewInstance {
                    operator_email: format!("op{i}@example.test"),
                    ..new_instance(&format!("org-{i}"))
                };
                setup::complete(&pool, &new).await
            }));
        }
        let (mut ok, mut already) = (0, 0);
        for handle in handles {
            match handle.await.unwrap() {
                Ok(_) => ok += 1,
                Err(SetupError::AlreadyCompleted) => already += 1,
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }
        assert_eq!((ok, already), (1, 9));
        assert_eq!(count(&fresh, "users").await, 1);
        assert_eq!(count(&fresh, "instance_setup").await, 1);
    }

    async fn user_with_hash(pool: &PgPool, email: &str, hash: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, normalized_email, display_name, password_hash) VALUES ($1, $2, 'U', $3)",
        )
        .bind(id)
        .bind(email)
        .bind(hash)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    fn new_session(user_id: Uuid, marker: u8) -> NewSession {
        let now = time::OffsetDateTime::now_utc();
        NewSession {
            user_id,
            token_hash: [marker; 32],
            now,
            absolute_expires_at: now + time::Duration::days(7),
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn verified_login_insert_requires_the_current_hash_and_an_enabled_account() {
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "u@example.test", "h1").await;
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 1), "h1")
                .await
                .unwrap()
                .is_some()
        );
        // A hash that is not the stored one (for example the one that was verified
        // before a reset committed) creates nothing.
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 2), "h0")
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("UPDATE users SET password_hash = 'h2' WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 3), "h1")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 4), "h2")
                .await
                .unwrap()
                .is_some()
        );
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(id, 5), "h2")
                .await
                .unwrap()
                .is_none()
        );
        // Unknown user.
        assert!(
            sessions::create_for_verified_login(&db.pool, &new_session(Uuid::new_v4(), 6), "h2")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(count(&db.pool, "sessions").await, 2);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn verified_login_insert_waits_for_a_password_change_in_flight_and_then_refuses() {
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "race@example.test", "h1").await;

        // A reset/password change has updated the row but not committed yet.
        let mut tx = db.pool.begin().await.unwrap();
        sqlx::query("UPDATE users SET password_hash = 'h2' WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();

        // The login verified "h1" earlier and now tries to create its session.
        let pool = db.pool.clone();
        let login = tokio::spawn(async move {
            sessions::create_for_verified_login(&pool, &new_session(id, 9), "h1").await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !login.is_finished(),
            "the insert must wait for the row lock"
        );

        tx.commit().await.unwrap();
        let outcome = login.await.unwrap().unwrap();
        assert!(
            outcome.is_none(),
            "a session for the replaced password was created"
        );
        assert_eq!(count(&db.pool, "sessions").await, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_change_requires_the_verified_hash_and_a_live_session() {
        use craftcloud_db::accounts;
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "chg@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        let keep = sessions::create_for_verified_login(&db.pool, &new_session(id, 1), "h1")
            .await
            .unwrap()
            .unwrap();
        let other = sessions::create_for_verified_login(&db.pool, &new_session(id, 2), "h1")
            .await
            .unwrap()
            .unwrap();
        sqlx::query("INSERT INTO password_reset_tokens (id, user_id, token_hash, created_at, expires_at) VALUES (gen_random_uuid(), $1, decode(repeat('cd', 32), 'hex'), now(), now() + interval '1 hour')")
            .bind(id)
            .execute(&db.pool)
            .await
            .unwrap();

        // A stale verified hash (a reset committed meanwhile): nothing changes.
        assert_eq!(
            accounts::change_password(&db.pool, id, "h0", "h3", keep, now)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h1")
        );
        // The kept session was revoked meanwhile (link issuance, logout): nothing changes,
        // including the password.
        sessions::revoke(&db.pool, keep, now).await.unwrap();
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", keep, now)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h1")
        );
        // A session of another user cannot be used as the kept session.
        let stranger = user_with_hash(&db.pool, "stranger@example.test", "x").await;
        let foreign = sessions::create_for_verified_login(&db.pool, &new_session(stranger, 3), "x")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", foreign, now)
                .await
                .unwrap(),
            None
        );

        // With a live session and the verified hash it works and cleans up.
        let live = sessions::create_for_verified_login(&db.pool, &new_session(id, 4), "h1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", live, now)
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h3")
        );
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(active, 1, "only the kept session stays active");
        let pending: i64 =
            sqlx::query_scalar("SELECT count(*) FROM password_reset_tokens WHERE used_at IS NULL")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(pending, 0);
        let _ = other;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn password_change_racing_a_reset_link_issue_changes_nothing() {
        use craftcloud_db::{accounts, password_reset};
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "race2@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        let keep = sessions::create_for_verified_login(&db.pool, &new_session(id, 1), "h1")
            .await
            .unwrap()
            .unwrap();

        // The issuance holds the user row lock (not yet committed): it will
        // revoke every session of the user.
        let mut issue = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
            .bind(id)
            .execute(&mut *issue)
            .await
            .unwrap();

        let pool = db.pool.clone();
        let change = tokio::spawn(async move {
            accounts::change_password(&pool, id, "h1", "h3", keep, now).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !change.is_finished(),
            "the change must wait for the issuance"
        );

        sqlx::query(
            "UPDATE sessions SET revoked_at = $2 WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(now)
        .execute(&mut *issue)
        .await
        .unwrap();
        issue.commit().await.unwrap();

        // The user row was locked but not changed, so the hash matches; the
        // session was revoked meanwhile, so the change is refused and rolled back.
        assert_eq!(change.await.unwrap().unwrap(), None);
        assert_eq!(
            accounts::password_hash(&db.pool, id)
                .await
                .unwrap()
                .as_deref(),
            Some("h1")
        );

        // The real issuance function produces the same outcome in sequence.
        let live = sessions::create_for_verified_login(&db.pool, &new_session(id, 2), "h1")
            .await
            .unwrap()
            .unwrap();
        let outcome = password_reset::create(
            &db.pool,
            &password_reset::NewResetToken {
                user_id: id,
                token_hash: [7; 32],
                created_by: None,
                now,
                expires_at: now + time::Duration::hours(1),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            outcome,
            password_reset::IssueOutcome::Issued {
                revoked_sessions: 1
            }
        ));
        assert_eq!(
            accounts::change_password(&db.pool, id, "h1", "h3", live, now)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_consume_issue_and_change_never_deadlock() {
        use craftcloud_db::{accounts, password_reset};
        let db = migrated().await;
        let id = user_with_hash(&db.pool, "dl@example.test", "h1").await;
        let now = time::OffsetDateTime::now_utc();
        for round in 0..40u8 {
            let issue = |marker: u8| {
                let pool = db.pool.clone();
                async move {
                    password_reset::create(
                        &pool,
                        &password_reset::NewResetToken {
                            user_id: id,
                            token_hash: [marker; 32],
                            created_by: None,
                            now,
                            expires_at: now + time::Duration::hours(1),
                        },
                    )
                    .await
                }
            };
            issue(round.wrapping_mul(2)).await.unwrap();
            let live = {
                let hash = accounts::password_hash(&db.pool, id)
                    .await
                    .unwrap()
                    .unwrap();
                (
                    sessions::create_for_verified_login(
                        &db.pool,
                        &new_session(id, 200 + (round % 50)),
                        &hash,
                    )
                    .await
                    .unwrap(),
                    hash,
                )
            };
            let (pool_a, pool_b, pool_c) = (db.pool.clone(), db.pool.clone(), db.pool.clone());
            let token = [round.wrapping_mul(2); 32];
            let current = live.1.clone();
            let keep = live.0;
            let consume = tokio::spawn(async move {
                password_reset::consume(&pool_a, &token, "h-consumed", now).await
            });
            let create = tokio::spawn(async move {
                password_reset::create(
                    &pool_b,
                    &password_reset::NewResetToken {
                        user_id: id,
                        token_hash: [round.wrapping_mul(2).wrapping_add(1); 32],
                        created_by: None,
                        now,
                        expires_at: now + time::Duration::hours(1),
                    },
                )
                .await
            });
            let change = tokio::spawn(async move {
                match keep {
                    Some(keep) => {
                        accounts::change_password(&pool_c, id, &current, "h-changed", keep, now)
                            .await
                            .map(|_| ())
                    }
                    None => Ok(()),
                }
            });
            // Any deadlock would surface as an internal error here.
            consume.await.unwrap().unwrap();
            create.await.unwrap().unwrap();
            change.await.unwrap().unwrap();
            // Reset the hash so the next round starts from a known state.
            sqlx::query("UPDATE users SET password_hash = 'h1' WHERE id = $1")
                .bind(id)
                .execute(&db.pool)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn removing_a_member_removes_their_workspace_grants_and_scopes_are_checked() {
        use craftcloud_db::members::{OrgTx, RemoveOutcome, RoleOutcome};
        let db = migrated().await;
        let (scope, owner) = org(&db.pool, "acme").await;
        let (other_scope, _) = org(&db.pool, "other").await;
        let member = user(&db.pool, "m@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            member.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        let ws = workspaces::create(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceCreate),
            &new_ws("main"),
        )
        .await
        .unwrap();
        workspaces::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::WorkspaceMemberAdd),
            ws.id,
            member.id,
            WorkspaceRole::Editor,
        )
        .await
        .unwrap();

        // A scope of another organization is refused by the transaction.
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(tx.remove(&other_scope, member.id).await.is_err());
        // A role change keeps the workspace grants; removal drops them.
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                member.id,
                OrgRole::Admin
            )
            .await
            .unwrap(),
            RoleOutcome::Changed(_)
        ));
        assert_eq!(count(&db.pool, "workspace_members").await, 1);
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                member.id
            )
            .await
            .unwrap(),
            RemoveOutcome::Removed
        );
        tx.commit().await.unwrap();
        assert_eq!(count(&db.pool, "workspace_members").await, 0);
        assert_eq!(count(&db.pool, "organization_members").await, 2);
        // The sole owner is protected, a missing member is reported.
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                owner.id
            )
            .await
            .unwrap(),
            RemoveOutcome::LastOwner
        );
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                member.id
            )
            .await
            .unwrap(),
            RemoveOutcome::NotMember
        );
        // Unknown organizations have no transaction.
        assert!(
            OrgTx::begin(&db.pool, Uuid::new_v4())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_creation_with_owner_is_atomic_and_account_state_changes_are_guarded() {
        use craftcloud_db::accounts::{self, DisableOutcome};
        use craftcloud_db::organizations::CreateOutcome;
        let db = migrated().await;
        let owner = user(&db.pool, "own@example.test").await;
        let new = |slug: &str, owner_user_id: Uuid| NewOrganization {
            slug: slug.to_owned(),
            name: "N".to_owned(),
            owner_user_id,
        };
        let created = organizations::create_with_owner(&db.pool, &new("one", owner.id))
            .await
            .unwrap();
        let CreateOutcome::Created(org) = created else {
            panic!("not created")
        };
        assert_eq!(org.version, 1);
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        // Duplicate slug: nothing is stored (no orphan membership).
        let dup = organizations::create_with_owner(&db.pool, &new("one", owner.id))
            .await
            .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));
        assert_eq!(count(&db.pool, "organizations").await, 1);
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        assert!(matches!(
            organizations::create_with_owner(&db.pool, &new("two", Uuid::new_v4()))
                .await
                .unwrap(),
            CreateOutcome::OwnerNotFound
        ));

        // Disabling is idempotent and never applies to the setup operator.
        let now = time::OffsetDateTime::now_utc();
        assert_eq!(
            accounts::set_disabled(&db.pool, owner.id, true, now)
                .await
                .unwrap(),
            DisableOutcome::Changed {
                orphaned_organizations: vec![org.id]
            }
        );
        assert_eq!(
            accounts::set_disabled(&db.pool, owner.id, true, now)
                .await
                .unwrap(),
            DisableOutcome::Unchanged {
                orphaned_organizations: vec![org.id]
            }
        );
        assert!(matches!(
            organizations::create_with_owner(&db.pool, &new("three", owner.id))
                .await
                .unwrap(),
            CreateOutcome::OwnerDisabled
        ));
        assert_eq!(
            accounts::set_disabled(&db.pool, owner.id, false, now)
                .await
                .unwrap(),
            DisableOutcome::Changed {
                orphaned_organizations: vec![]
            }
        );
        assert_eq!(
            accounts::set_disabled(&db.pool, Uuid::new_v4(), true, now)
                .await
                .unwrap(),
            DisableOutcome::NoSuchUser
        );
        let operator = setup::complete(&db.pool, &new_instance("operator-org"))
            .await
            .unwrap()
            .user_id;
        assert_eq!(
            accounts::set_disabled(&db.pool, operator, true, now)
                .await
                .unwrap(),
            DisableOutcome::IsOperator
        );
        assert_eq!(
            accounts::set_disabled(&db.pool, operator, false, now)
                .await
                .unwrap(),
            DisableOutcome::Unchanged {
                orphaned_organizations: vec![]
            }
        );
    }

    async fn disable(db: &TestDb, user: Uuid) {
        use craftcloud_db::accounts;
        accounts::set_disabled(&db.pool, user, true, time::OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabled_owners_do_not_count_for_the_last_owner_rule() {
        use craftcloud_db::members::{OrgTx, RemoveOutcome, RoleOutcome};
        let db = migrated().await;
        let (scope, x) = org(&db.pool, "acme").await;
        let y = user(&db.pool, "y@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            y.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        disable(&db, y.id).await;

        // Only a disabled owner would remain: the enabled owner can neither leave nor be demoted.
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                x.id,
                OrgRole::Admin
            )
            .await
            .unwrap(),
            RoleOutcome::LastOwner
        ));
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                x.id
            )
            .await
            .unwrap(),
            RemoveOutcome::LastOwner
        );
        // The disabled owner itself can be demoted or removed (it does not count).
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                y.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            RoleOutcome::Changed(_)
        ));
        tx.commit().await.unwrap();

        // With a second enabled owner the first may leave.
        let z = user(&db.pool, "z@example.test").await;
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                z.id,
                OrgRole::Owner
            )
            .await
            .unwrap(),
            craftcloud_db::members::AddOutcome::Added(_)
        ));
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                x.id
            )
            .await
            .unwrap(),
            RemoveOutcome::Removed
        );
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabling_the_last_enabled_owner_is_allowed_and_reports_the_orphans() {
        use craftcloud_db::accounts::{self, DisableOutcome};
        let db = migrated().await;
        let (scope_a, a) = org(&db.pool, "org-a").await;
        let (scope_b, _) = org(&db.pool, "org-b").await;
        // `a` also co-owns org-b together with its own owner, so org-b is not orphaned.
        organizations::add_member(
            &db.pool,
            &sc(&scope_b, craftcloud_core::policy::Action::MemberAdd),
            a.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        let now = time::OffsetDateTime::now_utc();
        let outcome = accounts::set_disabled(&db.pool, a.id, true, now)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            DisableOutcome::Changed {
                orphaned_organizations: vec![scope_a.org_id()]
            }
        );
        // Idempotent, and the orphan list reflects the current state.
        let again = accounts::set_disabled(&db.pool, a.id, true, now)
            .await
            .unwrap();
        assert_eq!(
            again,
            DisableOutcome::Unchanged {
                orphaned_organizations: vec![scope_a.org_id()]
            }
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn break_glass_recovery_makes_an_enabled_user_owner() {
        use craftcloud_db::recovery::{SetOwnerOutcome, set_org_owner};
        let db = migrated().await;
        let (scope, owner) = org(&db.pool, "acme").await;
        let org_id = scope.org_id();
        disable(&db, owner.id).await;
        let newcomer = user(&db.pool, "new@example.test").await;
        let member = user(&db.pool, "member@example.test").await;
        let invited = user(&db.pool, "invited@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            member.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            invited.id,
            OrgRole::Admin,
            MemberStatus::Invited,
        )
        .await
        .unwrap();

        assert_eq!(
            set_org_owner(&db.pool, "acme", "new@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::Added {
                org_id,
                user_id: newcomer.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "new@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::AlreadyOwner {
                org_id,
                user_id: newcomer.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "member@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::Promoted {
                org_id,
                user_id: member.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "invited@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::Promoted {
                org_id,
                user_id: invited.id
            }
        );
        assert_eq!(
            set_org_owner(&db.pool, "acme", "nobody@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::UserNotFound
        );
        assert_eq!(
            set_org_owner(&db.pool, "no-such-org", "new@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::OrgNotFound
        );
        // A disabled account cannot be made owner.
        disable(&db, newcomer.id).await;
        let gone = user(&db.pool, "gone@example.test").await;
        disable(&db, gone.id).await;
        assert_eq!(
            set_org_owner(&db.pool, "acme", "gone@example.test")
                .await
                .unwrap(),
            SetOwnerOutcome::UserDisabled
        );
        let owners: i64 = sqlx::query_scalar("SELECT count(*) FROM organization_members m JOIN users u ON u.id = m.user_id WHERE m.org_id = $1 AND m.role = 'owner' AND m.status = 'active' AND u.disabled_at IS NULL").bind(org_id).fetch_one(&db.pool).await.unwrap();
        assert_eq!(owners, 2, "member and invited users are now enabled owners");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn non_active_membership_rows_are_handled_explicitly() {
        use craftcloud_db::members::{self, AddOutcome, OrgTx, RemoveOutcome, RoleOutcome};
        let db = migrated().await;
        let (scope, _owner) = org(&db.pool, "acme").await;
        let (invited, disabled, old_owner) = (
            user(&db.pool, "inv@example.test").await,
            user(&db.pool, "dis@example.test").await,
            user(&db.pool, "oldowner@example.test").await,
        );
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            invited.id,
            OrgRole::Admin,
            MemberStatus::Invited,
        )
        .await
        .unwrap();
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            disabled.id,
            OrgRole::Member,
            MemberStatus::Disabled,
        )
        .await
        .unwrap();
        organizations::add_member(
            &db.pool,
            &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
            old_owner.id,
            OrgRole::Owner,
            MemberStatus::Disabled,
        )
        .await
        .unwrap();

        // Non-active rows are invisible as members ...
        assert_eq!(
            organizations::active_role(&db.pool, scope.org_id(), invited.id)
                .await
                .unwrap(),
            None
        );
        let listed = members::list(&db.pool, &scope, 50, None).await.unwrap();
        assert_eq!(listed.len(), 1, "only the active owner is listed");
        let mut tx = OrgTx::begin(&db.pool, scope.org_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tx.role_of(invited.id).await.unwrap(), None);
        // ... but cleanup tools see the stored role.
        assert_eq!(
            tx.stored_role_of(invited.id).await.unwrap(),
            Some(OrgRole::Admin)
        );
        // Changing the role of a non-active row is "not a member".
        assert!(matches!(
            tx.set_role(
                &sc(&scope, craftcloud_core::policy::Action::MemberChangeRole),
                invited.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            RoleOutcome::NotMember
        ));

        // Adding reactivates a non-owner row with the given role ...
        let AddOutcome::Added(m) = tx
            .add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                invited.id,
                OrgRole::Member,
            )
            .await
            .unwrap()
        else {
            panic!("not added")
        };
        assert_eq!(m.role, "member");
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                disabled.id,
                OrgRole::Admin
            )
            .await
            .unwrap(),
            AddOutcome::Added(_)
        ));
        assert_eq!(tx.role_of(disabled.id).await.unwrap(), Some(OrgRole::Admin));
        // ... an active member stays "already a member" ...
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                invited.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            AddOutcome::AlreadyMember
        ));
        // ... and a non-active owner row is never reactivated through add.
        assert!(matches!(
            tx.add(
                &sc(&scope, craftcloud_core::policy::Action::MemberAdd),
                old_owner.id,
                OrgRole::Member
            )
            .await
            .unwrap(),
            AddOutcome::AlreadyMember
        ));
        assert_eq!(tx.role_of(old_owner.id).await.unwrap(), None);
        // Remove deletes a row of any status.
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                old_owner.id
            )
            .await
            .unwrap(),
            RemoveOutcome::Removed
        );
        assert_eq!(
            tx.remove(
                &sc(&scope, craftcloud_core::policy::Action::MemberRemove),
                old_owner.id
            )
            .await
            .unwrap(),
            RemoveOutcome::NotMember
        );
        tx.commit().await.unwrap();
        assert_eq!(count(&db.pool, "organization_members").await, 3);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_scope_only_drives_the_repository_function_of_its_own_action() {
        use craftcloud_core::policy::Action;
        use craftcloud_db::members::OrgTx;
        let db = migrated().await;
        let (base, _) = org(&db.pool, "acme").await;
        let org_id = base.org_id();
        let (read, remove_scope, add_scope) = (
            read_scope_for(org_id),
            sc(&base, Action::MemberRemove),
            sc(&base, Action::MemberAdd),
        );
        let stranger = user(&db.pool, "s@example.test").await;

        let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
        // A read permit drives no write ...
        assert!(tx.add(&read, stranger.id, OrgRole::Member).await.is_err());
        assert!(
            tx.update_settings(&read, Some("Renamed"), None, 1)
                .await
                .is_err()
        );
        // ... and neither does the permit of another write action.
        assert!(
            tx.add(&remove_scope, stranger.id, OrgRole::Member)
                .await
                .is_err()
        );
        assert!(
            tx.set_role(&remove_scope, stranger.id, OrgRole::Admin)
                .await
                .is_err()
        );
        assert!(
            tx.update_settings(&add_scope, Some("Renamed"), None, 1)
                .await
                .is_err()
        );
        assert!(tx.remove(&add_scope, stranger.id).await.is_err());
        drop(tx);
        assert!(
            workspaces::create(&db.pool, &add_scope, &new_ws("main"))
                .await
                .is_err()
        );
        assert!(
            workspaces::create(&db.pool, &read, &new_ws("main"))
                .await
                .is_err()
        );
        assert!(
            organizations::add_member(
                &db.pool,
                &remove_scope,
                stranger.id,
                OrgRole::Member,
                MemberStatus::Active
            )
            .await
            .is_err()
        );
        assert_eq!(count(&db.pool, "organization_members").await, 1);
        assert_eq!(count(&db.pool, "workspaces").await, 0);
        // The matching scope works; reads work with any scope.
        let mut tx = OrgTx::begin(&db.pool, org_id).await.unwrap().unwrap();
        assert!(
            tx.add(&add_scope, stranger.id, OrgRole::Member)
                .await
                .is_ok()
        );
        assert!(tx.remove(&remove_scope, stranger.id).await.is_ok());
        assert!(
            tx.update_settings(
                &sc(&base, Action::OrganizationUpdate),
                Some("Renamed"),
                None,
                1
            )
            .await
            .is_ok()
        );
        tx.commit().await.unwrap();
        assert!(organizations::get(&db.pool, &read).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn the_last_owner_check_waits_for_a_concurrent_account_disable() {
        use craftcloud_core::policy::Action;
        use craftcloud_db::members::{OrgTx, RoleOutcome};
        let db = migrated().await;
        let (base, x) = org(&db.pool, "acme").await;
        let y = user(&db.pool, "y@example.test").await;
        organizations::add_member(
            &db.pool,
            &sc(&base, Action::MemberAdd),
            y.id,
            OrgRole::Owner,
            MemberStatus::Active,
        )
        .await
        .unwrap();

        // The other owner's account is being disabled: the transaction holds the
        // user row and has changed it, but has not committed yet.
        let mut disabling = db.pool.begin().await.unwrap();
        sqlx::query("SELECT 1 FROM users WHERE id = $1 FOR UPDATE")
            .bind(y.id)
            .execute(&mut *disabling)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(y.id)
            .execute(&mut *disabling)
            .await
            .unwrap();

        // x demotes themselves; with two owners (one about to be disabled) that looks allowed
        // unless the check waits for the disable and re-reads it.
        let pool = db.pool.clone();
        let scope = sc(&base, Action::MemberChangeRole);
        let org_id = base.org_id();
        let demote = tokio::spawn(async move {
            let mut tx = OrgTx::begin(&pool, org_id).await.unwrap().unwrap();
            tx.set_role(&scope, x.id, OrgRole::Admin).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !demote.is_finished(),
            "the owner count must wait for the in-flight disable"
        );

        disabling.commit().await.unwrap();
        let outcome = demote.await.unwrap().unwrap();
        assert!(matches!(outcome, RoleOutcome::LastOwner), "{outcome:?}");
        assert_eq!(
            organizations::active_role(&db.pool, org_id, x.id)
                .await
                .unwrap(),
            Some(OrgRole::Owner)
        );
    }

    // ---- audit events (CC-F11) -------------------------------------------------

    mod audit_tests {
        use super::*;
        use craftcloud_core::policy::Action;
        use craftcloud_db::audit::{
            self, AggScope, AuditCtx, AuditFilter, AuditResult, Detail, NewEvent, record_aggregated,
        };
        use craftcloud_db::members::OrgTx;
        use time::OffsetDateTime;

        async fn rows(pool: &PgPool, sql: &str) -> i64 {
            sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
        }

        /// Seeds `n` events of the organization with one shared timestamp.
        async fn seed_equal(pool: &PgPool, org: Uuid, n: i32, at: OffsetDateTime, action: &str) {
            sqlx::query(
                "INSERT INTO audit_events (id, org_id, action, result, created_at, last_at) \
                 SELECT gen_random_uuid(), $1, $2, 'success', $3, $3 FROM generate_series(1, $4)",
            )
            .bind(org)
            .bind(action)
            .bind(at)
            .bind(n)
            .execute(pool)
            .await
            .unwrap();
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn rows_cannot_be_updated_deleted_or_truncated() {
            let db = migrated().await;
            let ctx = AuditCtx::system();
            audit::record(&db.pool, &ctx, &NewEvent::new("organization.create"))
                .await
                .unwrap();
            for sql in [
                "UPDATE audit_events SET action = 'member.add'",
                "UPDATE audit_events SET result = 'failed'",
                "UPDATE audit_events SET details = '{\"x\":1}'::jsonb",
                "UPDATE audit_events SET event_count = event_count + 1",
                "DELETE FROM audit_events",
                "TRUNCATE audit_events",
            ] {
                let err = sqlx::query(sql).execute(&db.pool).await.unwrap_err();
                let text = err.to_string();
                assert!(
                    text.contains("append-only")
                        || text.contains("audit_purge")
                        || text.contains("truncated"),
                    "{sql}: {text}"
                );
            }
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 1);
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn invalid_values_are_rejected_by_the_database() {
            let db = migrated().await;
            for (action, result) in [
                ("Member.Add", "success"),
                ("member add", "success"),
                ("member.add", "maybe"),
                ("", "success"),
            ] {
                let outcome = sqlx::query(
                    "INSERT INTO audit_events (id, action, result) VALUES (gen_random_uuid(), $1, $2)",
                )
                .bind(action)
                .bind(result)
                .execute(&db.pool)
                .await;
                assert!(outcome.is_err(), "{action:?} {result:?}");
            }
            // A request id with odd characters is refused as well.
            let outcome = sqlx::query(
                "INSERT INTO audit_events (id, action, result, request_id) \
                 VALUES (gen_random_uuid(), 'member.add', 'success', 'a b')",
            )
            .execute(&db.pool)
            .await;
            assert!(outcome.is_err());
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn details_take_only_typed_values_with_plain_keys() {
            let db = migrated().await;
            let ctx = AuditCtx::system();
            let bad_key = NewEvent::new("member.add").detail("E-Mail", Detail::Flag(true));
            assert!(matches!(
                audit::record(&db.pool, &ctx, &bad_key).await,
                Err(DbError::CheckViolation { .. })
            ));
            let user_id = Uuid::new_v4();
            let ok = NewEvent::new("member.add")
                .detail("role", Detail::Code("admin"))
                .detail("count", Detail::Count(3))
                .detail("self_removal", Detail::Flag(false))
                .detail("user_id", Detail::Id(user_id));
            audit::record(&db.pool, &ctx, &ok).await.unwrap();
            let details: String = sqlx::query_scalar("SELECT details::text FROM audit_events")
                .fetch_one(&db.pool)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(&details).unwrap();
            assert_eq!(value["role"], "admin");
            assert_eq!(value["count"], 3);
            assert_eq!(value["self_removal"], false);
            assert_eq!(value["user_id"], user_id.to_string());
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn an_event_in_the_transaction_rolls_back_with_it() {
            let db = migrated().await;
            let (scope, owner) = org(&db.pool, "a1").await;
            let ctx = AuditCtx {
                actor_id: Some(owner.id),
                request_id: Some("req-1".to_owned()),
            };
            // Rolled back: no event.
            let mut tx = OrgTx::begin(&db.pool, scope.org_id())
                .await
                .unwrap()
                .unwrap();
            tx.audit(&ctx, NewEvent::new("member.add").target("user", owner.id))
                .await
                .unwrap();
            drop(tx);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 0);
            // Committed: exactly one event, bound to the transaction's organization.
            let mut tx = OrgTx::begin(&db.pool, scope.org_id())
                .await
                .unwrap()
                .unwrap();
            tx.audit(&ctx, NewEvent::new("member.add").target("user", owner.id))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            let (org_id, actor, request): (Option<Uuid>, Option<Uuid>, Option<String>) =
                sqlx::query_as("SELECT org_id, actor_id, request_id FROM audit_events")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!(
                (org_id, actor, request.as_deref()),
                (Some(scope.org_id()), Some(owner.id), Some("req-1"))
            );
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn aggregated_events_share_a_row_and_count_concurrent_writers() {
            let db = migrated().await;
            let pool = db.pool.clone();
            let actor = Uuid::new_v4();
            let mut tasks = Vec::new();
            for _ in 0..20 {
                let pool = pool.clone();
                tasks.push(tokio::spawn(async move {
                    let ctx = AuditCtx {
                        actor_id: Some(actor),
                        request_id: None,
                    };
                    record_aggregated(
                        &pool,
                        &ctx,
                        &NewEvent::new("auth.login").result(AuditResult::Failed),
                        AggScope::Actor,
                    )
                    .await
                    .unwrap();
                }));
            }
            for task in tasks {
                task.await.unwrap();
            }
            let (n, count): (i64, i32) =
                sqlx::query_as("SELECT count(*), max(event_count) FROM audit_events")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!((n, count), (1, 20));
            // The counter may only grow; no other column may change.
            for sql in [
                "UPDATE audit_events SET event_count = 1",
                "UPDATE audit_events SET result = 'success'",
            ] {
                assert!(sqlx::query(sql).execute(&db.pool).await.is_err(), "{sql}");
            }
            // Another actor gets its own row.
            record_aggregated(
                &db.pool,
                &AuditCtx {
                    actor_id: Some(Uuid::new_v4()),
                    request_id: None,
                },
                &NewEvent::new("auth.login").result(AuditResult::Failed),
                AggScope::Actor,
            )
            .await
            .unwrap();
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 2);
            // With the target in the key, the same actor and action split per target.
            let ctx = AuditCtx {
                actor_id: None,
                request_id: None,
            };
            for target in [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()] {
                for _ in 0..2 {
                    record_aggregated(
                        &db.pool,
                        &ctx,
                        &NewEvent::new("file.download").target("file_version", target),
                        AggScope::Target,
                    )
                    .await
                    .unwrap();
                }
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'file.download'"
                )
                .await,
                3
            );
            // Without it, probing many targets stays one row.
            for _ in 0..5 {
                record_aggregated(
                    &db.pool,
                    &ctx,
                    &NewEvent::new("member.change_role")
                        .result(AuditResult::Denied)
                        .target("user", Uuid::new_v4()),
                    AggScope::Actor,
                )
                .await
                .unwrap();
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'member.change_role'"
                )
                .await,
                1
            );
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn retention_purge_deletes_old_events_in_batches_only() {
            let db = migrated().await;
            let (scope, _) = org(&db.pool, "b1").await;
            let org_id = scope.org_id();
            let now = OffsetDateTime::now_utc();
            seed_equal(
                &db.pool,
                org_id,
                25,
                now - time::Duration::days(400),
                "file.purge",
            )
            .await;
            seed_equal(
                &db.pool,
                org_id,
                5,
                now - time::Duration::days(100),
                "file.purge",
            )
            .await;
            seed_equal(&db.pool, org_id, 3, now, "file.purge").await;
            assert_eq!(audit::count_expired(&db.pool, 365).await.unwrap(), 25);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 10);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 10);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 5);
            assert_eq!(audit::purge(&db.pool, 365, 10).await.unwrap(), 0);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 8);
            // The delete guard is lifted only inside the purge function.
            assert!(
                sqlx::query("DELETE FROM audit_events")
                    .execute(&db.pool)
                    .await
                    .is_err()
            );
            // A shorter retention reaches the 100 day old events.
            assert_eq!(audit::purge(&db.pool, 30, 100).await.unwrap(), 5);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 3);
            // Whatever cutoff is passed, nothing newer than 30 days goes.
            let removed: i64 =
                sqlx::query_scalar("SELECT audit_purge('infinity'::timestamptz, 1000)")
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            assert_eq!(removed, 0);
            assert_eq!(rows(&db.pool, "SELECT count(*) FROM audit_events").await, 3);
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn paging_is_stable_under_equal_timestamps_and_filters_narrow() {
            let db = migrated().await;
            let (scope, _) = org(&db.pool, "c1").await;
            let (other, _) = org(&db.pool, "c2").await;
            let at = OffsetDateTime::now_utc() - time::Duration::hours(1);
            seed_equal(&db.pool, scope.org_id(), 25, at, "member.add").await;
            seed_equal(&db.pool, scope.org_id(), 4, at, "workspace.member.add").await;
            seed_equal(&db.pool, other.org_id(), 6, at, "member.add").await;
            let read = sc(&scope, Action::AuditRead);

            // Walk all pages of 7: every event once, strictly descending (time, id).
            let mut seen = Vec::new();
            let mut after = None;
            loop {
                // One more than the page size tells whether another page follows.
                let page = audit::list(&db.pool, &read, &AuditFilter::default(), 8, after)
                    .await
                    .unwrap();
                let more = page.len() > 7;
                let take = page.len().min(7);
                seen.extend(page.iter().take(take).map(|e| (e.created_at, e.id)));
                if !more {
                    break;
                }
                after = page.get(take - 1).map(|e| (e.created_at, e.id));
            }
            assert_eq!(seen.len(), 29, "only this organization's events");
            let mut sorted = seen.clone();
            sorted.sort_by(|a, b| b.cmp(a));
            sorted.dedup();
            assert_eq!(sorted, seen, "no repeats, descending order");

            let prefix = AuditFilter {
                action_prefix: Some("member".to_owned()),
                ..AuditFilter::default()
            };
            assert_eq!(
                audit::list(&db.pool, &read, &prefix, 100, None)
                    .await
                    .unwrap()
                    .len(),
                25,
                "`member` does not match `workspace.member.add`"
            );
            let nested = AuditFilter {
                action_prefix: Some("workspace".to_owned()),
                ..AuditFilter::default()
            };
            assert_eq!(
                audit::list(&db.pool, &read, &nested, 100, None)
                    .await
                    .unwrap()
                    .len(),
                4
            );
            // `_` in a prefix is no wildcard.
            let underscore = AuditFilter {
                action_prefix: Some("workspace.member_add".to_owned()),
                ..AuditFilter::default()
            };
            assert!(
                audit::list(&db.pool, &read, &underscore, 100, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let window = AuditFilter {
                from: Some(at + time::Duration::seconds(1)),
                ..AuditFilter::default()
            };
            assert!(
                audit::list(&db.pool, &read, &window, 100, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let result = AuditFilter {
                result: Some(AuditResult::Denied),
                ..AuditFilter::default()
            };
            assert!(
                audit::list(&db.pool, &read, &result, 100, None)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let bad = AuditFilter {
                action_prefix: Some("a%".to_owned()),
                ..AuditFilter::default()
            };
            assert!(audit::list(&db.pool, &read, &bad, 10, None).await.is_err());
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn listing_needs_the_audit_scope_and_instance_events_stay_apart() {
            let db = migrated().await;
            let (scope, _) = org(&db.pool, "d1").await;
            audit::record(
                &db.pool,
                &AuditCtx::system(),
                &NewEvent::new("storage.rebind"),
            )
            .await
            .unwrap();
            audit::record(
                &db.pool,
                &AuditCtx::system(),
                &NewEvent::new("file.purge").org(scope.org_id()),
            )
            .await
            .unwrap();
            // A scope of another action cannot read the log.
            let wrong = sc(&scope, Action::MemberAdd);
            assert!(
                audit::list(&db.pool, &wrong, &AuditFilter::default(), 10, None)
                    .await
                    .is_err()
            );
            let read = sc(&scope, Action::AuditRead);
            let own = audit::list(&db.pool, &read, &AuditFilter::default(), 10, None)
                .await
                .unwrap();
            assert_eq!(own.len(), 1);
            assert_eq!(own[0].action, "file.purge");
            let instance = audit::list_instance(&db.pool, &AuditFilter::default(), 10, None)
                .await
                .unwrap();
            assert_eq!(instance.len(), 1);
            assert_eq!(instance[0].action, "storage.rebind");
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn repository_changes_record_exactly_one_event_and_replays_none() {
            use craftcloud_db::accounts;
            use craftcloud_db::password_reset::{self, NewResetToken};
            let db = migrated().await;
            let user = user(&db.pool, "u1@example.test").await;
            let ctx = AuditCtx {
                actor_id: Some(Uuid::new_v4()),
                request_id: Some("r-1".to_owned()),
            };
            let now = OffsetDateTime::now_utc();
            // Disable: one event; the same state again: none.
            for _ in 0..2 {
                accounts::set_disabled_audited(&db.pool, user.id, true, now, &ctx)
                    .await
                    .unwrap();
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'user.disable'"
                )
                .await,
                1
            );
            accounts::set_disabled_audited(&db.pool, user.id, false, now, &ctx)
                .await
                .unwrap();
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action = 'user.enable'"
                )
                .await,
                1
            );
            // A reset link: issue once, use once; a replay of the token records nothing.
            let hash = [7_u8; 32];
            password_reset::create_audited(
                &db.pool,
                &NewResetToken {
                    user_id: user.id,
                    token_hash: hash,
                    created_by: None,
                    now,
                    expires_at: now + time::Duration::hours(1),
                },
                &ctx,
            )
            .await
            .unwrap();
            for _ in 0..2 {
                password_reset::consume_audited(&db.pool, &hash, "hash-value", now, &ctx)
                    .await
                    .unwrap();
            }
            assert_eq!(
                rows(
                    &db.pool,
                    "SELECT count(*) FROM audit_events WHERE action IN ('password_reset.issue', 'password_reset.use')"
                )
                .await,
                2
            );
            // Nothing sensitive reached the table.
            let dump: String = sqlx::query_scalar(
                "SELECT COALESCE(string_agg(row_to_json(a)::text, ' '), '') FROM audit_events a",
            )
            .fetch_one(&db.pool)
            .await
            .unwrap();
            for needle in ["example.test", "hash-value", "@"] {
                assert!(!dump.contains(needle), "{needle} in {dump}");
            }
        }

        #[tokio::test]
        #[ignore = "requires PostgreSQL: cargo xtask db-test"]
        async fn the_runtime_role_cannot_bypass_the_audit_guard() {
            let admin = PgConnectOptions::from_str(&std::env::var(ENV).unwrap()).unwrap();
            let suffix = Uuid::new_v4().simple().to_string();
            let (owner, runtime, other) = (
                format!("cc_owner_{suffix}"),
                format!("cc_runtime_{suffix}"),
                format!("cc_other_{suffix}"),
            );
            let (member, sup, runtime2) = (
                format!("cc_member_{suffix}"),
                format!("cc_super_{suffix}"),
                format!("cc_runtime2_{suffix}"),
            );
            let db_name = format!("cc_test_{suffix}");
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            for sql in [
                format!("CREATE ROLE \"{owner}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{runtime}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{other}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{member}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE ROLE \"{sup}\" LOGIN SUPERUSER PASSWORD 'pw'"),
                format!("CREATE ROLE \"{runtime2}\" LOGIN PASSWORD 'pw'"),
                format!("CREATE DATABASE \"{db_name}\" OWNER \"{owner}\""),
            ] {
                sqlx::query(&sql).execute(&mut conn).await.unwrap();
            }
            let pool_of = |role: &str| {
                let options = admin
                    .clone()
                    .username(role)
                    .password("pw")
                    .database(&db_name);
                async move {
                    PgPoolOptions::new()
                        .max_connections(2)
                        .acquire_timeout(Duration::from_secs(10))
                        .connect_with(options)
                        .await
                        .unwrap()
                }
            };
            let owner_pool = pool_of(&owner).await;
            migrate::run(&owner_pool).await.unwrap();
            migrate::grant_runtime_role(&owner_pool, &runtime)
                .await
                .unwrap();
            // The grant step is repeatable and refuses odd role names.
            migrate::grant_runtime_role(&owner_pool, &runtime)
                .await
                .unwrap();
            assert!(
                migrate::grant_runtime_role(&owner_pool, "x\"; DROP TABLE users; --")
                    .await
                    .is_err()
            );
            let rt = pool_of(&runtime).await;
            let stranger = pool_of(&other).await;

            // The server's role starts, reads and writes everything it needs.
            migrate::verify(&rt).await.unwrap();
            assert_eq!(
                migrate::audit_guard_is_advisory(&rt).await.unwrap(),
                Some(false)
            );
            assert_eq!(
                migrate::audit_guard_is_advisory(&owner_pool).await.unwrap(),
                Some(true)
            );
            let (scope, _) = org(&rt, "r1").await;
            audit::record(
                &rt,
                &AuditCtx::system(),
                &NewEvent::new("file.purge").org(scope.org_id()),
            )
            .await
            .unwrap();
            let now = OffsetDateTime::now_utc();
            for (days, n) in [(400, 5), (100, 5)] {
                seed_equal(
                    &owner_pool,
                    scope.org_id(),
                    n,
                    now - time::Duration::days(days),
                    "file.purge",
                )
                .await;
            }

            // An insert cannot forge the time or the counter.
            sqlx::query(
                "INSERT INTO audit_events (id, action, result, created_at, last_at, event_count) \
                 VALUES (gen_random_uuid(), 'file.purge', 'success', '2000-01-01', '2000-01-01', 99)",
            )
            .execute(&rt)
            .await
            .unwrap();
            assert_eq!(
                rows(&rt, "SELECT count(*) FROM audit_events WHERE created_at < '2001-01-01' OR event_count = 99").await,
                0,
                "the insert guard resets time and counter"
            );

            // ... but cannot touch the history of the log or the table itself.
            for sql in [
                "DELETE FROM audit_events",
                "TRUNCATE audit_events",
                "UPDATE audit_events SET action = 'member.add'",
                "ALTER TABLE audit_events DISABLE TRIGGER ALL",
                "ALTER TABLE audit_events DISABLE TRIGGER audit_events_no_update_delete",
                "DROP TRIGGER audit_events_no_update_delete ON audit_events",
                "DROP TABLE audit_events",
                "SET session_replication_role = replica",
                "DELETE FROM _sqlx_migrations",
                "CREATE OR REPLACE FUNCTION audit_purge(timestamptz, integer) RETURNS bigint AS 'SELECT 0::bigint' LANGUAGE sql",
            ] {
                assert!(
                    sqlx::query(sql).execute(&rt).await.is_err(),
                    "the runtime role ran: {sql}"
                );
            }
            // Setting the old session switch changes nothing for this role.
            let mut one = rt.acquire().await.unwrap();
            sqlx::query("SELECT set_config('craftcloud.audit_purge', 'on', false)")
                .execute(&mut *one)
                .await
                .unwrap();
            assert!(
                sqlx::query("DELETE FROM audit_events")
                    .execute(&mut *one)
                    .await
                    .is_err()
            );
            drop(one);
            assert_eq!(rows(&rt, "SELECT count(*) FROM audit_events").await, 12);

            // Retention goes through `audit_purge`, which never reaches below 30 days,
            // whatever cutoff is passed.
            let removed: i64 =
                sqlx::query_scalar("SELECT audit_purge('infinity'::timestamptz, 1000)")
                    .fetch_one(&rt)
                    .await
                    .unwrap();
            assert_eq!(removed, 10);
            assert_eq!(rows(&rt, "SELECT count(*) FROM audit_events").await, 2);
            // A role without the grant cannot call it.
            assert!(
                sqlx::query("SELECT audit_purge('infinity'::timestamptz, 1)")
                    .execute(&stranger)
                    .await
                    .is_err()
            );
            // Nor can it read or write the table (no grant at all).
            assert!(
                sqlx::query("SELECT 1 FROM audit_events")
                    .execute(&stranger)
                    .await
                    .is_err()
            );

            // Roles that could undo the guard are refused, and the grant checks itself.
            sqlx::query(&format!("GRANT \"{owner}\" TO \"{member}\""))
                .execute(&mut conn)
                .await
                .unwrap();
            assert!(matches!(
                migrate::grant_runtime_role(&owner_pool, &member).await,
                Err(migrate::GrantError::Privileged(_))
            ));
            assert!(matches!(
                migrate::grant_runtime_role(&owner_pool, &sup).await,
                Err(migrate::GrantError::Privileged(_))
            ));
            assert_eq!(
                migrate::grant_runtime_role(&owner_pool, &owner).await,
                Err(migrate::GrantError::SameAsOwner)
            );
            assert_eq!(
                migrate::grant_runtime_role(&owner_pool, &format!("nobody_{suffix}")).await,
                Err(migrate::GrantError::UnknownRole)
            );
            // The startup check: a member of the owner and a superuser are warned about.
            let member_pool = pool_of(&member).await;
            assert_eq!(
                migrate::audit_guard_is_advisory(&member_pool)
                    .await
                    .unwrap(),
                Some(true)
            );
            let sup_pool = pool_of(&sup).await;
            assert_eq!(
                migrate::audit_guard_is_advisory(&sup_pool).await.unwrap(),
                Some(true)
            );
            // A migrating role that does not own the tables cannot grant: the
            // statements only warn, so the check afterwards fails the call.
            // (it has some rights on the objects, so PostgreSQL only warns)
            for sql in [
                "GRANT SELECT ON ALL TABLES IN SCHEMA public TO PUBLIC".to_owned(),
                "GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO PUBLIC".to_owned(),
            ] {
                sqlx::query(&sql).execute(&owner_pool).await.unwrap();
            }
            sqlx::query("GRANT EXECUTE ON FUNCTION audit_purge(timestamptz, integer) TO PUBLIC")
                .execute(&owner_pool)
                .await
                .unwrap();
            assert!(matches!(
                migrate::grant_runtime_role(&stranger, &runtime2).await,
                Err(migrate::GrantError::PrivilegesNotApplied(_))
            ));

            for pool in [rt, stranger, owner_pool, member_pool, sup_pool] {
                pool.close().await;
            }
            for sql in [
                format!("DROP DATABASE \"{db_name}\" WITH (FORCE)"),
                format!("DROP ROLE \"{owner}\""),
                format!("DROP ROLE \"{runtime}\""),
                format!("DROP ROLE \"{other}\""),
                format!("DROP ROLE \"{member}\""),
                format!("DROP ROLE \"{sup}\""),
                format!("DROP ROLE \"{runtime2}\""),
            ] {
                sqlx::query(&sql).execute(&mut conn).await.unwrap();
            }
        }
    }
}
