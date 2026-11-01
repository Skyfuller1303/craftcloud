//! PostgreSQL integration tests for the AC-10 review fixes: domain ownership,
//! provider MFA scoping, session binding lifecycle and pending-login recovery.

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use craftcloud_db::{
        audit::AuditCtx,
        migrate, oidc, oidc_state, password_reset, scim,
        setup::{self, NewInstance},
        sso,
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

    struct World {
        db: TestDb,
        operator: Uuid,
        org_a: Uuid,
        org_b: Uuid,
    }

    fn ctx(actor: Option<Uuid>) -> AuditCtx {
        AuditCtx {
            actor_id: actor,
            request_id: None,
        }
    }

    async fn world() -> World {
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
        let org_a: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        let org_b = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organizations(id,slug,name,owner_user_id) VALUES($1,'org-b','Org B',$2)",
        )
        .bind(org_b)
        .bind(instance.user_id)
        .execute(&db.pool)
        .await
        .unwrap();
        World {
            db,
            operator: instance.user_id,
            org_a,
            org_b,
        }
    }

    async fn configure(
        w: &World,
        org: Uuid,
        domains: &[&str],
        jit: bool,
        required: bool,
        trust: bool,
    ) {
        let mut tx = w.db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        sso::save_configuration(
            &mut tx,
            &sso::Configuration {
                org_id: org,
                discovery_url: "https://idp.example.test/.well-known/openid-configuration"
                    .to_owned(),
                client_id: "client".to_owned(),
                client_secret_ciphertext: vec![0xab; 40],
                allowed_domains: domains.iter().map(|d| (*d).to_owned()).collect(),
                enabled: true,
                jit_enabled: jit,
                sso_required: required,
                trust_mfa: trust,
                version: 1,
            },
            &ctx(None),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    async fn version(w: &World, org: Uuid) -> i64 {
        sqlx::query_scalar("SELECT version FROM organization_sso WHERE org_id=$1")
            .bind(org)
            .fetch_one(&w.db.pool)
            .await
            .unwrap()
    }

    fn identity(subject: &str, email: &str, mfa: bool) -> oidc::Identity {
        oidc::Identity {
            issuer: "https://idp.example.test".to_owned(),
            subject: subject.to_owned(),
            email: Some(email.to_owned()),
            email_verified: true,
            display_name: None,
            mfa,
        }
    }

    async fn resolve(
        w: &World,
        org: Uuid,
        id: &oidc::Identity,
        password_hash: Option<&str>,
    ) -> oidc::Resolution {
        let mut tx = w.db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(org)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        let v = version(w, org).await;
        let result = oidc::resolve_identity(
            &mut tx,
            org,
            v,
            id,
            password_hash,
            OffsetDateTime::now_utc(),
            &ctx(None),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        result
    }

    async fn audit_count(w: &World, action: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE action=$1")
            .bind(action)
            .fetch_one(&w.db.pool)
            .await
            .unwrap()
    }

    async fn new_member(w: &World, org: Uuid, email: &str) -> Uuid {
        let user = Uuid::new_v4();
        sqlx::query("INSERT INTO users(id,normalized_email,display_name,email_verified_at) VALUES($1,$2,'Member',now())")
            .bind(user)
            .bind(email)
            .execute(&w.db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(org)
            .bind(user)
            .execute(&w.db.pool)
            .await
            .unwrap();
        user
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn unapproved_domains_are_inert_and_approval_is_audited_and_unique() {
        let w = world().await;
        configure(&w, w.org_a, &["example.test"], true, false, true).await;
        configure(&w, w.org_b, &["example.test"], true, false, true).await;
        let mut conn = w.db.pool.acquire().await.unwrap();
        let claims = sso::claims_for_org(&mut conn, w.org_a).await.unwrap();
        drop(conn);
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].status, "pending");

        // Pending claims: discovery, JIT creation and SCIM provisioning are all inert.
        assert_eq!(
            sso::discover_domain(&w.db.pool, "example.test")
                .await
                .unwrap(),
            None
        );
        let squatter = identity("squat-1", "ceo@example.test", false);
        assert_eq!(
            resolve(&w, w.org_a, &squatter, None).await,
            oidc::Resolution::Rejected
        );
        let created: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM users WHERE normalized_email='ceo@example.test'",
        )
        .fetch_one(&w.db.pool)
        .await
        .unwrap();
        assert_eq!(
            created, 0,
            "an unapproved domain must not pre-create accounts"
        );
        let input = scim::UserInput {
            user_name: "victim@example.test".to_owned(),
            display_name: "Victim".to_owned(),
            external_id: None,
            active: true,
        };
        let mut tx = w.db.pool.begin().await.unwrap();
        let refused = scim::create_user(
            &mut tx,
            w.org_a,
            &input,
            OffsetDateTime::now_utc(),
            &ctx(None),
        )
        .await;
        assert!(
            matches!(refused, Err(craftcloud_db::DbError::CheckViolation { .. })),
            "{refused:?}"
        );
        tx.rollback().await.unwrap();

        // Domains are stored lower-case only.
        let upper =
            sqlx::query("INSERT INTO sso_domain_claims(org_id,domain) VALUES($1,'Other.Test')")
                .bind(w.org_a)
                .execute(&w.db.pool)
                .await;
        assert!(upper.is_err());

        // Only an active instance operator decides.
        let stranger = new_member(&w, w.org_a, "stranger@elsewhere.test").await;
        assert!(
            sso::decide_claim(
                &w.db.pool,
                w.org_a,
                "example.test",
                true,
                &ctx(Some(stranger))
            )
            .await
            .is_err()
        );
        assert_eq!(audit_count(&w, "sso.domain_approved").await, 0);

        assert_eq!(
            sso::decide_claim(
                &w.db.pool,
                w.org_a,
                "example.test",
                true,
                &ctx(Some(w.operator))
            )
            .await
            .unwrap(),
            sso::ClaimDecision::Applied
        );
        assert_eq!(audit_count(&w, "sso.domain_approved").await, 1);
        let actor: Option<Uuid> = sqlx::query_scalar(
            "SELECT actor_id FROM audit_events WHERE action='sso.domain_approved'",
        )
        .fetch_one(&w.db.pool)
        .await
        .unwrap();
        assert_eq!(actor, Some(w.operator));
        assert_eq!(
            sso::discover_domain(&w.db.pool, "example.test")
                .await
                .unwrap(),
            Some(w.org_a)
        );
        assert_eq!(
            sso::discover_domain(&w.db.pool, "EXAMPLE.test")
                .await
                .unwrap(),
            Some(w.org_a)
        );

        // Approved: JIT and SCIM work for the owner of the claim only.
        let resolved = resolve(&w, w.org_a, &squatter, None).await;
        let oidc::Resolution::Resolved(user) = resolved else {
            panic!("approved domain must admit: {resolved:?}");
        };
        let verified: bool =
            sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM users WHERE id=$1")
                .bind(user)
                .fetch_one(&w.db.pool)
                .await
                .unwrap();
        assert!(verified);
        let mut tx = w.db.pool.begin().await.unwrap();
        scim::create_user(
            &mut tx,
            w.org_a,
            &input,
            OffsetDateTime::now_utc(),
            &ctx(None),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        // The same domain cannot be approved for a second organization, which stays inert.
        assert_eq!(
            sso::decide_claim(
                &w.db.pool,
                w.org_b,
                "example.test",
                true,
                &ctx(Some(w.operator))
            )
            .await
            .unwrap(),
            sso::ClaimDecision::DomainTaken
        );
        assert_eq!(audit_count(&w, "sso.domain_approved").await, 1);
        let other = identity("squat-2", "intruder@example.test", false);
        assert_eq!(
            resolve(&w, w.org_b, &other, None).await,
            oidc::Resolution::Rejected
        );
        assert_eq!(
            sso::discover_domain(&w.db.pool, "example.test")
                .await
                .unwrap(),
            Some(w.org_a)
        );
        assert_eq!(
            sso::decide_claim(
                &w.db.pool,
                w.org_b,
                "missing.test",
                true,
                &ctx(Some(w.operator))
            )
            .await
            .unwrap(),
            sso::ClaimDecision::NotFound
        );

        // Removing the domain from the configuration drops the approval with it.
        configure(&w, w.org_a, &["other-domain.test"], true, false, true).await;
        assert_eq!(
            sso::discover_domain(&w.db.pool, "example.test")
                .await
                .unwrap(),
            None
        );
        let pending = sso::claims_by_status(&w.db.pool, "pending", 10)
            .await
            .unwrap();
        assert!(pending.iter().any(|c| c.domain == "other-domain.test"));
        // Rejecting a claim is audited separately from approving it.
        assert_eq!(
            sso::decide_claim(
                &w.db.pool,
                w.org_b,
                "example.test",
                false,
                &ctx(Some(w.operator))
            )
            .await
            .unwrap(),
            sso::ClaimDecision::Applied
        );
        assert_eq!(audit_count(&w, "sso.domain_rejected").await, 1);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn provider_mfa_never_replaces_a_local_factor_or_its_lockout_or_crosses_organizations() {
        let w = world().await;
        configure(&w, w.org_a, &["example.test"], true, false, true).await;
        configure(&w, w.org_b, &["example.test"], true, false, true).await;
        sqlx::query("UPDATE organizations SET require_two_factor=true WHERE id=$1")
            .bind(w.org_b)
            .execute(&w.db.pool)
            .await
            .unwrap();
        let va = version(&w, w.org_a).await;
        let now = OffsetDateTime::now_utc();
        let browser = |n: u8| oidc::SessionInput::Browser {
            token_hash: [n; 32],
            absolute_expires_at: now + time::Duration::hours(1),
        };

        // A member with a local TOTP factor still has to prove it after provider MFA.
        let with_totp = new_member(&w, w.org_a, "totp@example.test").await;
        sqlx::query("INSERT INTO user_two_factor(user_id,secret_ciphertext,enabled_at,created_at) VALUES($1,decode(repeat('ab',40),'hex'),now(),now())")
            .bind(with_totp)
            .execute(&w.db.pool)
            .await
            .unwrap();
        let mut tx = w.db.pool.begin().await.unwrap();
        let outcome = oidc::issue_session(
            &mut tx,
            w.org_a,
            with_totp,
            va,
            true,
            None,
            None,
            &browser(1),
            now,
            &ctx(None),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, oidc::LoginOutcome::FactorRequired);
        tx.rollback().await.unwrap();

        // A locked-out factor stays locked even with provider MFA.
        sqlx::query("UPDATE user_two_factor SET failed_attempts=$2 WHERE user_id=$1")
            .bind(with_totp)
            .bind(craftcloud_db::two_factor::MAX_FAILED_ATTEMPTS)
            .execute(&w.db.pool)
            .await
            .unwrap();
        let mut tx = w.db.pool.begin().await.unwrap();
        let outcome = oidc::issue_session(
            &mut tx,
            w.org_a,
            with_totp,
            va,
            true,
            None,
            None,
            &browser(2),
            now,
            &ctx(None),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, oidc::LoginOutcome::Rejected);
        tx.rollback().await.unwrap();

        // Without a local factor, provider MFA counts in the binding organization only.
        let plain = new_member(&w, w.org_a, "plain@example.test").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','active')")
            .bind(w.org_b)
            .bind(plain)
            .execute(&w.db.pool)
            .await
            .unwrap();
        let mut tx = w.db.pool.begin().await.unwrap();
        let outcome = oidc::issue_session(
            &mut tx,
            w.org_a,
            plain,
            va,
            true,
            None,
            None,
            &browser(3),
            now,
            &ctx(None),
            false,
        )
        .await
        .unwrap();
        let oidc::LoginOutcome::Created(session) = outcome else {
            panic!("{outcome:?}");
        };
        tx.commit().await.unwrap();
        let earlier = now - time::Duration::hours(1);
        assert_eq!(
            sso::session_policy(&w.db.pool, w.org_a, plain, "browser", session, earlier)
                .await
                .unwrap(),
            sso::SessionPolicy::Allowed {
                mfa: true,
                sso: true
            }
        );
        assert_eq!(
            sso::session_policy(&w.db.pool, w.org_b, plain, "browser", session, earlier)
                .await
                .unwrap(),
            sso::SessionPolicy::Allowed {
                mfa: false,
                sso: false
            },
            "provider MFA of one organization must not satisfy another organization"
        );
        // Another organization's two-factor requirement is not met by a login without MFA.
        let vb = version(&w, w.org_b).await;
        let mut tx = w.db.pool.begin().await.unwrap();
        let outcome = oidc::issue_session(
            &mut tx,
            w.org_b,
            plain,
            vb,
            false,
            None,
            None,
            &browser(4),
            now,
            &ctx(None),
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome, oidc::LoginOutcome::SetupRequired);
        tx.rollback().await.unwrap();

        // Instance operators are never linked to a tenant-controlled identity.
        sqlx::query("UPDATE users SET email_verified_at=now() WHERE id=$1")
            .bind(w.operator)
            .execute(&w.db.pool)
            .await
            .unwrap();
        assert_eq!(
            sso::decide_claim(
                &w.db.pool,
                w.org_a,
                "example.test",
                true,
                &ctx(Some(w.operator))
            )
            .await
            .unwrap(),
            sso::ClaimDecision::Applied
        );
        let hijack = identity("operator-subject", "op@example.test", true);
        assert_eq!(
            resolve(&w, w.org_a, &hijack, None).await,
            oidc::Resolution::Rejected
        );
        assert_eq!(
            resolve(&w, w.org_a, &hijack, Some("test-hash")).await,
            oidc::Resolution::Rejected
        );
        let linked: i64 =
            sqlx::query_scalar("SELECT count(*) FROM external_identities WHERE user_id=$1")
                .bind(w.operator)
                .fetch_one(&w.db.pool)
                .await
                .unwrap();
        assert_eq!(linked, 0);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn sso_required_does_not_lock_members_out_while_the_instance_switch_is_off() {
        let w = world().await;
        configure(&w, w.org_a, &["example.test"], false, true, true).await;
        let member = new_member(&w, w.org_a, "member@example.test").await;
        let created = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let session = Uuid::new_v4();
        let policy = || async {
            sso::session_policy(&w.db.pool, w.org_a, member, "browser", session, created)
                .await
                .unwrap()
        };
        assert_eq!(policy().await, sso::SessionPolicy::SsoRequired);
        sso::set_instance_enabled(&w.db.pool, false, &ctx(Some(w.operator)))
            .await
            .unwrap();
        assert_eq!(
            policy().await,
            sso::SessionPolicy::Allowed {
                mfa: false,
                sso: false
            }
        );
        sso::set_instance_enabled(&w.db.pool, true, &ctx(Some(w.operator)))
            .await
            .unwrap();
        assert_eq!(policy().await, sso::SessionPolicy::SsoRequired);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn session_bindings_are_removed_with_their_session() {
        let w = world().await;
        configure(&w, w.org_a, &["example.test"], false, false, true).await;
        let user = new_member(&w, w.org_a, "bound@example.test").await;
        let now = OffsetDateTime::now_utc();
        let browser = Uuid::new_v4();
        sqlx::query("INSERT INTO sessions(id,user_id,token_hash,created_at,last_seen_at,absolute_expires_at) VALUES($1,$2,$3,$4,$4,$5)")
            .bind(browser)
            .bind(user)
            .bind([7_u8; 32].as_slice())
            .bind(now)
            .bind(now + time::Duration::hours(1))
            .execute(&w.db.pool)
            .await
            .unwrap();
        let device = Uuid::new_v4();
        sqlx::query("INSERT INTO device_sessions(id,user_id,name,client_kind,client_id,refresh_hash,refresh_family,access_hash,access_expires_at,created_at,last_used_at,idle_expires_at,absolute_expires_at) VALUES($1,$2,'Dev','cli','craftcloud',$3,$4,$5,$6,$7,$7,$8,$8)")
            .bind(device)
            .bind(user)
            .bind([8_u8; 32].as_slice())
            .bind(Uuid::new_v4())
            .bind([9_u8; 32].as_slice())
            .bind(now + time::Duration::minutes(15))
            .bind(now)
            .bind(now + time::Duration::days(1))
            .execute(&w.db.pool)
            .await
            .unwrap();
        for (kind, id) in [("browser", browser), ("device", device)] {
            sqlx::query("INSERT INTO sso_session_bindings(session_kind,session_id,org_id,user_id,mfa,provider_mfa) VALUES($1,$2,$3,$4,false,false)")
                .bind(kind)
                .bind(id)
                .bind(w.org_a)
                .bind(user)
                .execute(&w.db.pool)
                .await
                .unwrap();
        }
        // A binding cannot point to a session that does not exist.
        let orphan = sqlx::query("INSERT INTO sso_session_bindings(session_kind,session_id,org_id,user_id,mfa,provider_mfa) VALUES('browser',$1,$2,$3,false,false)")
            .bind(Uuid::new_v4())
            .bind(w.org_a)
            .bind(user)
            .execute(&w.db.pool)
            .await;
        assert!(orphan.is_err());
        let count = |kind: &'static str| {
            let pool = w.db.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM sso_session_bindings WHERE session_kind=$1",
                )
                .bind(kind)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!((count("browser").await, count("device").await), (1, 1));
        sqlx::query("DELETE FROM sessions WHERE id=$1")
            .bind(browser)
            .execute(&w.db.pool)
            .await
            .unwrap();
        assert_eq!((count("browser").await, count("device").await), (0, 1));
        sqlx::query("DELETE FROM device_sessions WHERE id=$1")
            .bind(device)
            .execute(&w.db.pool)
            .await
            .unwrap();
        assert_eq!((count("browser").await, count("device").await), (0, 0));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn account_recovery_deletes_pending_logins_bound_to_the_account() {
        let w = world().await;
        configure(&w, w.org_a, &["example.test"], true, false, true).await;
        let victim = new_member(&w, w.org_a, "victim@example.test").await;
        sqlx::query("UPDATE users SET password_hash='old-hash' WHERE id=$1")
            .bind(victim)
            .execute(&w.db.pool)
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        let v = version(&w, w.org_a).await;
        let mut tx = w.db.pool.begin().await.unwrap();
        for (n, bind) in [(1_u8, true), (2_u8, false)] {
            assert!(
                oidc_state::insert_pending(
                    &mut tx,
                    &[n; 32],
                    w.org_a,
                    v,
                    &[9; 32],
                    &[0xcd; 40],
                    now,
                    600
                )
                .await
                .unwrap()
            );
            if bind {
                oidc_state::bind_user(&mut tx, &[n; 32], victim)
                    .await
                    .unwrap();
            }
        }
        tx.commit().await.unwrap();
        let bound: Option<Uuid> =
            sqlx::query_scalar("SELECT user_id FROM oidc_pending_logins WHERE token_hash=$1")
                .bind([1_u8; 32].as_slice())
                .fetch_one(&w.db.pool)
                .await
                .unwrap();
        assert_eq!(bound, Some(victim));

        let token = [5_u8; 32];
        password_reset::create(
            &w.db.pool,
            &password_reset::NewResetToken {
                user_id: victim,
                token_hash: token,
                created_by: Some(w.operator),
                reset_two_factor: false,
                now,
                expires_at: now + time::Duration::hours(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            password_reset::consume(&w.db.pool, &token, "new-hash", now)
                .await
                .unwrap(),
            Some(victim)
        );
        let left: Vec<Vec<u8>> =
            sqlx::query_scalar("SELECT token_hash FROM oidc_pending_logins ORDER BY token_hash")
                .fetch_all(&w.db.pool)
                .await
                .unwrap();
        assert_eq!(
            left,
            vec![[2_u8; 32].to_vec()],
            "only the unbound capability survives recovery"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn one_organization_filling_its_pending_budget_cannot_block_another() {
        let w = world().await;
        configure(&w, w.org_a, &["example.test"], true, false, true).await;
        configure(&w, w.org_b, &["other.test"], true, false, true).await;
        let now = OffsetDateTime::now_utc();
        let key = |n: u32| {
            let mut hash = [0_u8; 32];
            hash[..4].copy_from_slice(&n.to_be_bytes());
            hash
        };
        // Fill organization A's budget directly, then ask through the real admission path.
        sqlx::query("INSERT INTO oidc_transactions(state_hash,launch_hash,org_id,config_version,binding_hash,secrets_ciphertext,created_at,expires_at) SELECT decode(lpad(to_hex(g),64,'0'),'hex'),decode(lpad(to_hex(g+100000),64,'0'),'hex'),$1,1,decode(repeat('00',32),'hex'),decode(repeat('ab',40),'hex'),$2,$2+interval '10 minutes' FROM generate_series(1,1000) g")
            .bind(w.org_a)
            .bind(now)
            .execute(&w.db.pool)
            .await
            .unwrap();
        let admitted_a = oidc_state::insert_state(
            &w.db.pool,
            &key(2_000_000),
            &key(2_000_001),
            w.org_a,
            1,
            &[0; 32],
            &[0xcd; 40],
            now,
        )
        .await
        .unwrap();
        assert!(!admitted_a, "organization A is at its own limit");
        let admitted_b = oidc_state::insert_state(
            &w.db.pool,
            &key(3_000_000),
            &key(3_000_001),
            w.org_b,
            1,
            &[0; 32],
            &[0xcd; 40],
            now,
        )
        .await
        .unwrap();
        assert!(
            admitted_b,
            "another organization must still be admitted while A is full"
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn scim_authentication_takes_no_lock_before_the_token_is_valid() {
        let w = world().await;
        let token = "cc_scim_lock-order-token";
        let mut tx = w.db.pool.begin().await.unwrap();
        scim::rotate_token(&mut tx, w.org_a, &scim::token_hash(token))
            .await
            .unwrap();
        tx.commit().await.unwrap();
        // Another transaction holds the organization row lock, as a long SCIM write would.
        let mut holder = w.db.pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM organizations WHERE id=$1 FOR UPDATE")
            .bind(w.org_a)
            .fetch_one(&mut *holder)
            .await
            .unwrap();
        // Wrong and unknown credentials answer immediately instead of queueing on the lock.
        let quick = Duration::from_secs(2);
        assert!(
            tokio::time::timeout(quick, scim::begin(&w.db.pool, w.org_a, "cc_scim_wrong"))
                .await
                .expect("an invalid token must not wait for the organization lock")
                .unwrap()
                .is_none()
        );
        assert!(
            tokio::time::timeout(quick, scim::begin(&w.db.pool, Uuid::new_v4(), token))
                .await
                .expect("an unknown organization must not wait for any lock")
                .unwrap()
                .is_none()
        );
        // A valid token does serialize behind the holder and succeeds once it is released.
        let waiting = tokio::spawn({
            let pool = w.db.pool.clone();
            let org = w.org_a;
            async move { scim::begin(&pool, org, token).await.map(|t| t.is_some()) }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !waiting.is_finished(),
            "a valid token still serializes on the organization"
        );
        holder.rollback().await.unwrap();
        assert!(waiting.await.unwrap().unwrap());
    }
}
