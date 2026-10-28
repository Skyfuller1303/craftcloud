//! Integration tests against a real PostgreSQL server.
//!
//! Run with `cargo xtask db-test`, which starts a throwaway container and sets
//! `CRAFTCLOUD_TEST_DATABASE_URL`. Every test creates its own database
//! `cc_test_<uuid>` and drops it afterwards.

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::time::Duration;

    use craftcloud_db::migrate::{self, migrator_up_to};
    use craftcloud_db::organizations::{self, MemberStatus, NewOrganization, OrgRole};
    use craftcloud_db::setup::{self, NewInstance, SetupError};
    use craftcloud_db::users::{self, NewUser, User};
    use craftcloud_db::workspaces::{self, MAX_LIST_LIMIT, NewWorkspace, WorkspaceRole};
    use craftcloud_db::{DbError, OrgScope, PgPool, SchemaProblem};
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection, Row};
    use uuid::Uuid;

    const ENV: &str = "CRAFTCLOUD_TEST_DATABASE_URL";

    /// A throwaway database, dropped (with FORCE) when the guard is dropped.
    struct TestDb {
        name: String,
        admin: PgConnectOptions,
        pool: PgPool,
    }

    impl TestDb {
        async fn new() -> Self {
            let admin_url = std::env::var(ENV).expect("CRAFTCLOUD_TEST_DATABASE_URL must be set");
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
        let scope = OrgScope::caller_has_authorized(created.id);
        organizations::add_member(pool, &scope, owner.id, OrgRole::Owner, MemberStatus::Active)
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
            vec![1, 2, 3]
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
            3,
            "each migration applied exactly once in total"
        );
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 3);
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
            Err(DbError::Schema(SchemaProblem::Pending(vec![2, 3])))
        );

        let (scope, owner) = org(&db.pool, "acme").await;

        let second = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            second.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![2, 3]
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
        workspaces::create(&db.pool, &scope, &new_ws("main"))
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
        let ws = workspaces::create(&db.pool, &scope, &new_ws("main"))
            .await
            .unwrap();
        let outsider = user(&db.pool, "outsider@example.test").await;
        let err =
            workspaces::add_member(&db.pool, &scope, ws.id, outsider.id, WorkspaceRole::Viewer)
                .await
                .unwrap_err();
        assert_eq!(err.constraint(), Some("workspace_members_org_member_fk"));
        assert!(matches!(err, DbError::ForeignKeyViolation { .. }));

        // Becomes possible once the user is an organization member.
        organizations::add_member(
            &db.pool,
            &scope,
            outsider.id,
            OrgRole::Member,
            MemberStatus::Active,
        )
        .await
        .unwrap();
        workspaces::add_member(&db.pool, &scope, ws.id, outsider.id, WorkspaceRole::Viewer)
            .await
            .unwrap();
        let again =
            workspaces::add_member(&db.pool, &scope, ws.id, outsider.id, WorkspaceRole::Editor)
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
        let ws_a = workspaces::create(&db.pool, &scope_a, &new_ws("main"))
            .await
            .unwrap();
        // owner_b is an org-b member, but ws_a belongs to org-a.
        let err = workspaces::add_member(
            &db.pool,
            &scope_b,
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
        workspaces::create(&db.pool, &scope_a, &new_ws("main"))
            .await
            .unwrap();
        let dup = workspaces::create(&db.pool, &scope_a, &new_ws("main"))
            .await
            .unwrap_err();
        assert!(matches!(dup, DbError::UniqueViolation { .. }));
        assert_eq!(dup.constraint(), Some("workspaces_org_slug_unique"));
        workspaces::create(&db.pool, &scope_b, &new_ws("main"))
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
            &scope,
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
            &scope,
            &NewWorkspace {
                quota_bytes: Some(0),
                ..new_ws("zero")
            },
        )
        .await
        .unwrap();

        for bad_slug in ["Upper", "-lead", "has space", "", &"a".repeat(64)] {
            let err = workspaces::create(&db.pool, &scope, &new_ws(bad_slug))
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
        let ws_a = workspaces::create(&db.pool, &scope_a, &new_ws("a-one"))
            .await
            .unwrap();
        let ws_b = workspaces::create(&db.pool, &scope_b, &new_ws("b-one"))
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
        let ws = workspaces::create(&db.pool, &scope, &new_ws("main"))
            .await
            .unwrap();
        workspaces::add_member(&db.pool, &scope, ws.id, owner.id, WorkspaceRole::Admin)
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
        let (scope, _) = org(&db.pool, "acme").await;
        let ws = workspaces::create(&db.pool, &scope, &new_ws("main"))
            .await
            .unwrap();

        let applied = migrate::run(&db.pool).await.unwrap();
        assert_eq!(
            applied.iter().map(|a| a.version).collect::<Vec<_>>(),
            vec![3]
        );
        migrate::verify(&db.pool).await.unwrap();
        assert!(
            workspaces::get(&db.pool, &scope, ws.id)
                .await
                .unwrap()
                .is_some()
        );
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
}
