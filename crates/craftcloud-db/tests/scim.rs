//! PostgreSQL integration tests for SCIM tenant persistence (AC-10).

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use craftcloud_db::{
        audit::AuditCtx,
        migrate, oidc, scim,
        setup::{self, NewInstance},
    };
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use sqlx::{Connection, PgConnection};
    use time::OffsetDateTime;
    use uuid::Uuid;

    const ENV: &str = "CC_TEST_DATABASE_URL";

    struct TestDb {
        name: String,
        admin: PgConnectOptions,
        pool: craftcloud_db::PgPool,
    }

    impl TestDb {
        async fn new() -> Self {
            let admin = PgConnectOptions::from_str(
                &std::env::var(ENV).expect("CC_TEST_DATABASE_URL must be set"),
            )
            .unwrap();
            let name = format!("cc_test_{}", Uuid::new_v4().simple());
            let mut conn = PgConnection::connect_with(&admin).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{name}\""))
                .execute(&mut conn)
                .await
                .unwrap();
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
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                rt.block_on(async {
                    if let Ok(mut c) = PgConnection::connect_with(&admin).await {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        ))
                        .execute(&mut c)
                        .await;
                    }
                });
            })
            .join();
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn token_hash_rotation_and_resource_reads_are_organization_scoped() {
        let db = TestDb::new().await;
        let instance = setup::complete(
            &db.pool,
            &NewInstance {
                operator_email: "op@example.test".to_owned(),
                operator_display_name: "Operator".to_owned(),
                operator_password_hash: "test-hash".to_owned(),
                organization_name: "Operator Org".to_owned(),
                organization_slug: "operator-org".to_owned(),
            },
        )
        .await
        .unwrap();
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        let token = "cc_scim_synthetic-secret";
        let mut tx = db.pool.begin().await.unwrap();
        scim::rotate_token(&mut tx, org, &scim::token_hash(token))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(scim::begin(&db.pool, org, token).await.unwrap().is_some());
        assert!(
            scim::begin(&db.pool, org, "cc_scim_wrong-secret")
                .await
                .unwrap()
                .is_none()
        );
        let stored: Vec<u8> =
            sqlx::query_scalar("SELECT token_hash FROM scim_tokens WHERE org_id=$1")
                .bind(org)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_ne!(stored, token.as_bytes());
        let other_org = Uuid::new_v4();
        let input = scim::UserInput {
            user_name: "member@example.test".to_owned(),
            display_name: "Member".to_owned(),
            external_id: None,
            active: true,
        };
        let mut tx = db.pool.begin().await.unwrap();
        let result = scim::create_user(
            &mut tx,
            other_org,
            &input,
            OffsetDateTime::now_utc(),
            &AuditCtx::system(),
        )
        .await;
        assert!(result.is_err());
        tx.rollback().await.unwrap();
        assert_eq!(
            instance.user_id,
            sqlx::query_scalar::<_, Uuid>("SELECT owner_user_id FROM organizations WHERE id=$1")
                .bind(org)
                .fetch_one(&db.pool)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn group_mapping_reconciles_only_scim_owned_workspace_roles() {
        let db = TestDb::new().await;
        setup::complete(
            &db.pool,
            &NewInstance {
                operator_email: "op@example.test".to_owned(),
                operator_display_name: "Operator".to_owned(),
                operator_password_hash: "test-hash".to_owned(),
                organization_name: "Operator Org".to_owned(),
                organization_slug: "operator-org".to_owned(),
            },
        )
        .await
        .unwrap();
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        let input = scim::UserInput {
            user_name: "mapped@example.test".to_owned(),
            display_name: "Mapped".to_owned(),
            external_id: None,
            active: true,
        };
        let now = OffsetDateTime::now_utc();
        let mut tx = db.pool.begin().await.unwrap();
        let user = scim::create_user(&mut tx, org, &input, now, &AuditCtx::system())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let workspace = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces(id,org_id,slug,name) VALUES($1,$2,'ac10-workspace','AC10 Workspace')").bind(workspace).bind(org).execute(&db.pool).await.unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        let (group, _) = scim::create_group(
            &mut tx,
            org,
            &scim::GroupInput {
                external_id: None,
                display_name: "Mapped Group".to_owned(),
                members: vec![user.id],
            },
            now,
            &AuditCtx::system(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let mapping = scim::Mapping {
            group_id: group.id,
            workspace_id: workspace,
            role: "editor".to_owned(),
        };
        let mut tx = db.pool.begin().await.unwrap();
        scim::replace_mappings(&mut tx, org, &[mapping], now, &AuditCtx::system())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let role: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(org)
        .bind(workspace)
        .bind(user.user_id)
        .fetch_optional(&db.pool)
        .await
        .unwrap();
        assert_eq!(role.as_deref(), Some("editor"));
        sqlx::query("UPDATE workspace_members SET role='admin' WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3").bind(org).bind(workspace).bind(user.user_id).execute(&db.pool).await.unwrap();
        let mut tx = db.pool.begin().await.unwrap();
        scim::replace_mappings(&mut tx, org, &[], now, &AuditCtx::system())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let manual: Option<String> = sqlx::query_scalar(
            "SELECT role FROM workspace_members WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3",
        )
        .bind(org)
        .bind(workspace)
        .bind(user.user_id)
        .fetch_optional(&db.pool)
        .await
        .unwrap();
        let ownership: i64 = sqlx::query_scalar("SELECT count(*) FROM scim_workspace_ownership WHERE org_id=$1 AND workspace_id=$2 AND user_id=$3").bind(org).bind(workspace).bind(user.user_id).fetch_one(&db.pool).await.unwrap();
        assert_eq!(manual.as_deref(), Some("admin"));
        assert_eq!(ownership, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn scim_oidc_bridge_rechecks_password_after_recovery_lock() {
        let db = TestDb::new().await;
        setup::complete(
            &db.pool,
            &NewInstance {
                operator_email: "op@example.test".to_owned(),
                operator_display_name: "Operator".to_owned(),
                operator_password_hash: "test-hash".to_owned(),
                organization_name: "Operator Org".to_owned(),
                organization_slug: "operator-org".to_owned(),
            },
        )
        .await
        .unwrap();
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        let now = OffsetDateTime::now_utc();
        let mut create = db.pool.begin().await.unwrap();
        let member = scim::create_user(
            &mut create,
            org,
            &scim::UserInput {
                user_name: "member@example.test".to_owned(),
                display_name: "Member".to_owned(),
                external_id: Some("subject".to_owned()),
                active: true,
            },
            now,
            &AuditCtx::system(),
        )
        .await
        .unwrap();
        create.commit().await.unwrap();
        sqlx::query("UPDATE users SET email_verified_at=$2 WHERE id=$1")
            .bind(member.user_id)
            .bind(now)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organization_sso(org_id,discovery_url,client_id,client_secret_ciphertext,allowed_domains,enabled,jit_enabled,sso_required,trust_mfa) VALUES($1,'https://issuer.example.test','client',decode(repeat('ab',30),'hex'),ARRAY['example.test'],true,false,false,true)")
            .bind(org).execute(&db.pool).await.unwrap();

        let mut recovery = db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(member.user_id)
            .fetch_one(&mut *recovery)
            .await
            .unwrap();
        sqlx::query("UPDATE users SET password_hash='recovered-password' WHERE id=$1")
            .bind(member.user_id)
            .execute(&mut *recovery)
            .await
            .unwrap();

        let pool = db.pool.clone();
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        let resolve = tokio::spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            pid_tx.send(pid).unwrap();
            let result = oidc::resolve_identity(
                &mut tx,
                org,
                1,
                &oidc::Identity {
                    issuer: "https://issuer.example.test".to_owned(),
                    subject: "subject".to_owned(),
                    email: Some("member@example.test".to_owned()),
                    email_verified: true,
                    display_name: Some("Member".to_owned()),
                    mfa: false,
                },
                None,
                now,
                &AuditCtx::system(),
            )
            .await;
            match result {
                Ok(value) => {
                    tx.commit().await.unwrap();
                    value
                }
                Err(error) => {
                    tx.rollback().await.unwrap();
                    panic!("resolve failed: {error}")
                }
            }
        });
        let pid = pid_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let blocked: bool =
                    sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1)) > 0")
                        .bind(pid)
                        .fetch_one(&db.pool)
                        .await
                        .unwrap();
                if blocked {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("OIDC resolver never blocked on recovery transaction");
        recovery.commit().await.unwrap();
        assert_eq!(
            resolve.await.unwrap(),
            oidc::Resolution::LinkRequired(member.user_id)
        );
        let bindings: i64 = sqlx::query_scalar("SELECT count(*) FROM external_identities WHERE issuer='https://issuer.example.test' AND subject='subject'").fetch_one(&db.pool).await.unwrap();
        assert_eq!(bindings, 0);
    }
}
