//! HTTP-level tests of organizations, memberships and the RBAC policy
//! (CC-F04, CC-F05): cross-organization isolation, role rules, last-owner
//! protection, concurrency, CSRF/authentication, pagination, account
//! disabling and audit events. All tests need PostgreSQL
//! (`cargo xtask db-test`).

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::StatusCode;
    use serde_json::json;
    use uuid::Uuid;

    use crate::common::{COOKIE_NAME, PASSWORD, Req, Resp, Session, TestEnv, call, capture_logs};

    const SKIP: &str = "requires PostgreSQL: cargo xtask db-test";

    struct World {
        env: TestEnv,
        op: Session,
    }

    impl World {
        async fn new() -> Self {
            let env = TestEnv::new().await;
            let op = env.login_ok("op").await;
            Self { env, op }
        }

        /// Operator creates an organization for `owner`; returns its id.
        async fn org(&self, slug: &str, owner: Uuid) -> Uuid {
            let r = call(
                &self.env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&self.op)
                    .json(json!({"name": format!("Org {slug}"), "slug": slug, "owner_user_id": owner.to_string()})),
            )
            .await;
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.text);
            Uuid::parse_str(r.body["id"].as_str().unwrap()).unwrap()
        }

        async fn add(&self, actor: &Session, org: Uuid, user: Uuid, role: &str) -> Resp {
            call(
                &self.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                    .session(actor)
                    .json(json!({"user_id": user.to_string(), "role": role})),
            )
            .await
        }

        async fn set_role(&self, actor: &Session, org: Uuid, user: Uuid, role: &str) -> Resp {
            call(
                &self.env.app,
                Req::new(
                    "PATCH",
                    &format!("/api/v1/organizations/{org}/members/{user}"),
                )
                .session(actor)
                .json(json!({"role": role})),
            )
            .await
        }

        async fn remove(&self, actor: &Session, org: Uuid, user: Uuid) -> Resp {
            call(
                &self.env.app,
                Req::new(
                    "DELETE",
                    &format!("/api/v1/organizations/{org}/members/{user}"),
                )
                .session(actor),
            )
            .await
        }

        async fn members(&self, actor: &Session, org: Uuid) -> Resp {
            call(
                &self.env.app,
                Req::new("GET", &format!("/api/v1/organizations/{org}/members")).cookie_only(actor),
            )
            .await
        }

        async fn role_in_db(&self, org: Uuid, user: Uuid) -> Option<String> {
            sqlx::query_scalar(
                "SELECT role FROM organization_members WHERE org_id = $1 AND user_id = $2",
            )
            .bind(org)
            .bind(user)
            .fetch_optional(&self.env.pool)
            .await
            .unwrap()
        }

        async fn owners(&self, org: Uuid) -> i64 {
            self.env
                .count(&format!("SELECT count(*) FROM organization_members WHERE org_id = '{org}' AND role = 'owner' AND status = 'active'"))
                .await
        }
    }

    fn not_found(r: &Resp) {
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::NOT_FOUND, "NOT_FOUND"),
            "{}",
            r.text
        );
    }

    fn forbidden(r: &Resp) {
        assert_eq!(
            (r.status, r.code()),
            (StatusCode::FORBIDDEN, "FORBIDDEN"),
            "{}",
            r.text
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn admin_member_filters_are_private_and_cursor_bound() {
        let world = World::new().await;
        let owner_id = world.env.user("ac08-members-owner").await;
        let owner = world.env.login_ok("ac08-members-owner").await;
        let org = world
            .org(&format!("ac08-{}", Uuid::new_v4().simple()), owner_id)
            .await;
        let member_id = world.env.user("ac08-members-member").await;
        let member = world.env.login_ok("ac08-members-member").await;
        assert_eq!(
            world.add(&owner, org, member_id, "member").await.status,
            StatusCode::CREATED
        );
        let invited_id = world.env.user("ac08-members-invited").await;
        sqlx::query("INSERT INTO organization_members(org_id,user_id,role,status) VALUES($1,$2,'member','invited')")
            .bind(org).bind(invited_id).execute(&world.env.pool).await.unwrap();
        let first = call(
            &world.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{org}/members?status=all&sort=name_asc&limit=1"),
            )
            .session(&owner),
        )
        .await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.text);
        assert_eq!(first.body["items"].as_array().unwrap().len(), 1);
        assert!(first.body["items"][0]["status"].is_string());
        assert!(first.body["items"][0]["account_disabled"].is_boolean());
        assert!(first.body["items"][0]["two_factor_enabled"].is_boolean());
        let cursor = first.body["next_cursor"].as_str().unwrap();
        let second = call(&world.env.app, Req::new("GET", &format!("/api/v1/organizations/{org}/members?status=all&sort=name_asc&limit=1&cursor={cursor}")).session(&owner)).await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.text);
        assert_eq!(second.body["items"].as_array().unwrap().len(), 1);
        assert_ne!(
            first.body["items"][0]["user_id"],
            second.body["items"][0]["user_id"]
        );
        let old_shape = call(
            &world.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}/members")).session(&member),
        )
        .await;
        assert_eq!(old_shape.status, StatusCode::OK, "{}", old_shape.text);
        assert!(
            old_shape.body["items"][0]
                .get("two_factor_enabled")
                .is_none()
        );
        let denied = call(
            &world.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{org}/members?status=all"),
            )
            .session(&member),
        )
        .await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{}", denied.text);
        let invited = call(
            &world.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{org}/members?status=invited"),
            )
            .session(&owner),
        )
        .await;
        assert_eq!(invited.status, StatusCode::OK, "{}", invited.text);
        assert_eq!(invited.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(invited.body["items"][0]["user_id"], invited_id.to_string());
        assert_eq!(invited.body["items"][0]["status"], "invited");
        let disabled = call(
            &world.env.app,
            Req::new("POST", &format!("/api/v1/admin/users/{member_id}/disable"))
                .session(&world.op),
        )
        .await;
        assert_eq!(disabled.status, StatusCode::OK, "{}", disabled.text);
        let disabled_members = call(
            &world.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/organizations/{org}/members?status=disabled"),
            )
            .session(&owner),
        )
        .await;
        assert_eq!(
            disabled_members.status,
            StatusCode::OK,
            "{}",
            disabled_members.text
        );
        assert_eq!(disabled_members.body["items"].as_array().unwrap().len(), 1);
        assert_eq!(disabled_members.body["items"][0]["account_disabled"], true);
        let invalid_cursor = call(&world.env.app, Req::new("GET", &format!("/api/v1/organizations/{org}/members?status=invited&sort=name_asc&limit=1&cursor={cursor}")).session(&owner)).await;
        assert_eq!(
            invalid_cursor.status,
            StatusCode::BAD_REQUEST,
            "{}",
            invalid_cursor.text
        );
        assert_eq!(invalid_cursor.code(), "INVALID_CURSOR");
        assert_eq!(
            world.remove(&owner, org, invited_id).await.status,
            StatusCode::NO_CONTENT
        );
        let removed_cursor = call(&world.env.app, Req::new("GET", &format!("/api/v1/organizations/{org}/members?status=all&sort=name_asc&limit=1&cursor={cursor}")).session(&owner)).await;
        assert_eq!(
            removed_cursor.status,
            StatusCode::BAD_REQUEST,
            "{}",
            removed_cursor.text
        );
        assert_eq!(removed_cursor.code(), "INVALID_CURSOR");
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn operator_user_search_cursors_hash_arbitrary_filter_text() {
        let world = World::new().await;
        let pipe_users = [
            world.env.user("ac08-pipe-a").await,
            world.env.user("ac08-pipe-b").await,
        ];
        let emoji_users = [
            world.env.user("ac08-emoji-a").await,
            world.env.user("ac08-emoji-b").await,
        ];
        for (id, name) in pipe_users.iter().zip(["alpha|beta-one", "alpha|beta-two"]) {
            sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1")
                .bind(id)
                .bind(name)
                .execute(&world.env.pool)
                .await
                .unwrap();
        }
        let emoji = "😀".repeat(100);
        for id in emoji_users {
            sqlx::query("UPDATE users SET display_name=$2 WHERE id=$1")
                .bind(id)
                .bind(&emoji)
                .execute(&world.env.pool)
                .await
                .unwrap();
        }

        let emoji_query = "%F0%9F%98%80".repeat(100);
        for search in ["alpha%7Cbeta", &emoji_query] {
            let first = call(
                &world.env.app,
                Req::new(
                    "GET",
                    &format!("/api/v1/admin/users?limit=1&query={search}"),
                )
                .session(&world.op),
            )
            .await;
            assert_eq!(first.status, StatusCode::OK, "{}", first.text);
            let cursor = first.body["next_cursor"].as_str().expect("another result");
            let second = call(
                &world.env.app,
                Req::new(
                    "GET",
                    &format!("/api/v1/admin/users?limit=1&query={search}&cursor={cursor}"),
                )
                .session(&world.op),
            )
            .await;
            assert_eq!(second.status, StatusCode::OK, "{}", second.text);
            assert_ne!(first.body["items"][0]["id"], second.body["items"][0]["id"]);
        }

        // An out-of-range search term is reported as the `query` field.
        for bad in ["a", &"x".repeat(101)] {
            let rejected = call(
                &world.env.app,
                Req::new("GET", &format!("/api/v1/admin/users?query={bad}")).session(&world.op),
            )
            .await;
            assert_eq!(
                rejected.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{}",
                rejected.text
            );
            assert!(
                rejected.text.contains("\"query\"") && !rejected.text.contains("unverified"),
                "{}",
                rejected.text
            );
        }
    }

    // ---- operator: create / list ------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn only_the_operator_creates_organizations_and_lists_metadata() {
        let w = World::new().await;
        let alice = w.env.user("alice").await;
        let a = w.env.login_ok("alice").await;
        let body = json!({"name": "Acme", "slug": "acme", "owner_user_id": alice.to_string()});

        // A normal user (even an owner elsewhere) cannot create or list instance-wide.
        let denied = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&a)
                .json(body.clone()),
        )
        .await;
        forbidden(&denied);
        forbidden(
            &call(
                &w.env.app,
                Req::new("GET", "/api/v1/admin/organizations").cookie_only(&a),
            )
            .await,
        );
        assert_eq!(w.env.count("SELECT count(*) FROM organizations").await, 1);

        let created = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&w.op)
                .json(body.clone()),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        assert_eq!(created.body["owner_user_id"], json!(alice.to_string()));
        assert_eq!(created.body["version"], json!(1));
        assert_eq!(created.etag(), "\"org-v1\"");
        let org: Uuid = created.body["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(w.role_in_db(org, alice).await.as_deref(), Some("owner"));
        // The operator is not a member of what they created for others.
        assert_eq!(w.role_in_db(org, w.env.operator_id).await, None);

        // Validation: names the invalid fields only; the failed attempts create nothing.
        for (bad, fields) in [
            (
                json!({"name": "", "slug": "x", "owner_user_id": alice.to_string()}),
                json!(["name"]),
            ),
            (
                json!({"name": "N", "slug": "Bad Slug", "owner_user_id": alice.to_string()}),
                json!(["slug"]),
            ),
            (
                json!({"name": "N", "slug": "ok", "owner_user_id": "nope"}),
                json!(["owner_user_id"]),
            ),
            (
                json!({"name": "N", "slug": "ok", "owner_user_id": Uuid::new_v4().to_string()}),
                json!(["owner_user_id"]),
            ),
            (body.clone(), json!(["slug"])), // duplicate slug
        ] {
            let r = call(
                &w.env.app,
                Req::new("POST", "/api/v1/organizations")
                    .session(&w.op)
                    .json(bad),
            )
            .await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),
                "{}",
                r.text
            );
            assert_eq!(r.body["error"]["details"]["fields"], fields);
        }
        let disabled = w.env.user("gone").await;
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(disabled)
            .execute(&w.env.pool)
            .await
            .unwrap();
        let r = call(
            &w.env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&w.op)
                .json(json!({"name": "N", "slug": "dis", "owner_user_id": disabled.to_string()})),
        )
        .await;
        assert_eq!(
            r.body["error"]["details"]["fields"],
            json!(["owner_user_id"])
        );
        assert_eq!(w.env.count("SELECT count(*) FROM organizations").await, 2);

        // The operator may own an organization too.
        w.org("ops-own", w.env.operator_id).await;

        // Metadata list: exactly these fields, no member or user data.
        let list = call(
            &w.env.app,
            Req::new("GET", "/api/v1/admin/organizations").cookie_only(&w.op),
        )
        .await;
        assert_eq!(list.status, StatusCode::OK);
        let items = list.body["items"].as_array().unwrap();
        assert_eq!(items.len(), 3);
        for item in items {
            let mut keys: Vec<_> = item.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(
                keys,
                [
                    "committed_bytes",
                    "created_at",
                    "id",
                    "member_count",
                    "name",
                    "quota_bytes",
                    "reserved_bytes",
                    "slug"
                ]
            );
            assert_eq!(item["quota_bytes"], serde_json::Value::Null);
        }
        assert!(!list.text.contains("@example.test"));
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn listings_are_paginated_with_bound_cursors() {
        let w = World::new().await;
        let owner = w.env.user("owner").await;
        for i in 0..5 {
            w.org(&format!("org-{i}"), owner).await;
        }
        let s = w.env.login_ok("owner").await;

        // Own organizations: pages of 2.
        let mut seen = Vec::new();
        let mut url = "/api/v1/organizations?limit=2".to_owned();
        let mut pages = 0;
        loop {
            let r = call(&w.env.app, Req::new("GET", &url).cookie_only(&s)).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            let items = r.body["items"].as_array().unwrap();
            assert!(items.len() <= 2);
            seen.extend(items.iter().map(|i| i["id"].as_str().unwrap().to_owned()));
            pages += 1;
            match r.body["next_cursor"].as_str() {
                Some(cursor) => url = format!("/api/v1/organizations?limit=2&cursor={cursor}"),
                None => break,
            }
        }
        assert_eq!((seen.len(), pages), (5, 3));
        let unique: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), 5, "no duplicates across pages");

        // Cursors are bound to the listing and to the caller.
        let first = call(
            &w.env.app,
            Req::new("GET", "/api/v1/organizations?limit=1").cookie_only(&s),
        )
        .await;
        let cursor = first.body["next_cursor"].as_str().unwrap().to_owned();
        let other_listing = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!("/api/v1/admin/organizations?cursor={cursor}"),
            )
            .cookie_only(&w.op),
        )
        .await;
        assert_eq!(
            (other_listing.status, other_listing.code()),
            (StatusCode::BAD_REQUEST, "INVALID_CURSOR")
        );
        w.env.user("other").await;
        let other = w.env.login_ok("other").await;
        let replay = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations?cursor={cursor}")).cookie_only(&other),
        )
        .await;
        assert_eq!(replay.code(), "INVALID_CURSOR");

        // Bad paging parameters.
        for bad in ["limit=0", "limit=abc", "limit=-1", "bogus=1"] {
            let r = call(
                &w.env.app,
                Req::new("GET", &format!("/api/v1/organizations?{bad}")).cookie_only(&s),
            )
            .await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),
                "{bad}"
            );
        }
        let garbage = call(
            &w.env.app,
            Req::new("GET", "/api/v1/organizations?cursor=!!!").cookie_only(&s),
        )
        .await;
        assert_eq!(garbage.code(), "INVALID_CURSOR");
        // Large limits are clamped to the maximum page size.
        let big = call(
            &w.env.app,
            Req::new("GET", "/api/v1/organizations?limit=100000").cookie_only(&s),
        )
        .await;
        assert_eq!(big.status, StatusCode::OK);
        assert!(big.body["items"].as_array().unwrap().len() <= 100);

        // Admin list pages too.
        let admin = call(
            &w.env.app,
            Req::new("GET", "/api/v1/admin/organizations?limit=2").cookie_only(&w.op),
        )
        .await;
        assert_eq!(admin.body["items"].as_array().unwrap().len(), 2);
        assert!(admin.body["next_cursor"].is_string());
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn member_listing_is_paginated_in_join_order_and_cursors_hold_no_personal_data() {
        use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
        let w = World::new().await;
        let owner = w.env.user("zed").await;
        let org = w.org("pages", owner).await;
        let z = w.env.login_ok("zed").await;
        let long_cjk = "测".repeat(100);
        for name in ["bea", "cal", long_cjk.as_str(), "dan", "eve", "fay"] {
            let id = w.env.user(name).await;
            assert_eq!(
                w.add(&z, org, id, "member").await.status,
                StatusCode::CREATED
            );
        }
        let mut names = Vec::new();
        let mut cursors = Vec::new();
        let mut url = format!("/api/v1/organizations/{org}/members?limit=1");
        loop {
            let r = call(&w.env.app, Req::new("GET", &url).cookie_only(&z)).await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.text);
            names.extend(
                r.body["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["display_name"].as_str().unwrap().to_owned()),
            );
            match r.body["next_cursor"].as_str() {
                Some(cursor) => {
                    cursors.push(cursor.to_owned());
                    url = format!("/api/v1/organizations/{org}/members?limit=1&cursor={cursor}");
                }
                None => break,
            }
        }
        // Join order, every member exactly once, also across the long CJK name.
        assert_eq!(
            names,
            ["zed", "bea", "cal", long_cjk.as_str(), "dan", "eve", "fay"]
        );
        // No cursor contains a name, an e-mail address or any non-ASCII text.
        assert_eq!(cursors.len(), 6);
        for cursor in &cursors {
            assert!(cursor.is_ascii() && cursor.len() < 200, "{cursor}");
            let text = String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).unwrap()).unwrap();
            assert!(text.is_ascii());
            // (Names like "bea" are valid hex and could occur inside a UUID by chance,
            // so only names with non-hex letters are searched for.)
            assert!(!text.contains("example.test") && !text.contains("zed"));
            let fields: Vec<&str> = text.split('|').collect();
            assert_eq!(fields.len(), 5, "{text}");
            assert!(fields[3].parse::<i128>().is_ok(), "timestamp field: {text}");
            assert!(Uuid::parse_str(fields[4]).is_ok(), "id field: {text}");
        }

        // A cursor of this organization does not work against another one.
        let other_owner = w.env.user("other").await;
        let other_org = w.org("pages-2", other_owner).await;
        let o = w.env.login_ok("other").await;
        let replay = call(
            &w.env.app,
            Req::new(
                "GET",
                &format!(
                    "/api/v1/organizations/{other_org}/members?cursor={}",
                    cursors[0]
                ),
            )
            .cookie_only(&o),
        )
        .await;
        assert_eq!(replay.code(), "INVALID_CURSOR");
    }

    // ---- isolation --------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn a_member_of_one_organization_sees_nothing_of_another() {
        let w = World::new().await;
        let (alice, bob) = (w.env.user("alice").await, w.env.user("bob").await);
        let a_org = w.org("org-a", alice).await;
        let b_org = w.org("org-b", bob).await;
        let a = w.env.login_ok("alice").await;
        let b = w.env.login_ok("bob").await;
        let unknown = Uuid::new_v4();

        let routes = |org: Uuid| {
            vec![
                Req::new("GET", &format!("/api/v1/organizations/{org}")).cookie_only(&a),
                Req::new("PATCH", &format!("/api/v1/organizations/{org}"))
                    .session(&a)
                    .if_match("\"org-v1\"")
                    .json(json!({"name": "Hijacked"})),
                Req::new("GET", &format!("/api/v1/organizations/{org}/members")).cookie_only(&a),
                Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                    .session(&a)
                    .json(json!({"user_id": alice.to_string(), "role": "owner"})),
                Req::new(
                    "PATCH",
                    &format!("/api/v1/organizations/{org}/members/{bob}"),
                )
                .session(&a)
                .json(json!({"role": "member"})),
                Req::new(
                    "DELETE",
                    &format!("/api/v1/organizations/{org}/members/{bob}"),
                )
                .session(&a),
                Req::new(
                    "DELETE",
                    &format!("/api/v1/organizations/{org}/members/{alice}"),
                )
                .session(&a),
            ]
        };
        // Foreign organization and unknown id look exactly the same.
        let mut foreign = Vec::new();
        for req in routes(b_org) {
            foreign.push(call(&w.env.app, req).await);
        }
        let mut missing = Vec::new();
        for req in routes(unknown) {
            missing.push(call(&w.env.app, req).await);
        }
        for (f, m) in foreign.iter().zip(&missing) {
            not_found(f);
            not_found(m);
            assert_eq!(f.body["error"]["message"], m.body["error"]["message"]);
        }
        // The operator has no implicit access either.
        for path in [
            format!("/api/v1/organizations/{a_org}"),
            format!("/api/v1/organizations/{a_org}/members"),
        ] {
            not_found(&call(&w.env.app, Req::new("GET", &path).cookie_only(&w.op)).await);
        }
        not_found(&w.add(&w.op, a_org, bob, "member").await);
        not_found(&w.remove(&w.op, a_org, alice).await);
        not_found(
            &call(
                &w.env.app,
                Req::new("GET", "/api/v1/organizations/not-a-uuid").cookie_only(&a),
            )
            .await,
        );

        // IDOR: own organization id combined with a foreign user id.
        not_found(&w.set_role(&a, a_org, bob, "admin").await);
        not_found(&w.remove(&a, a_org, bob).await);
        // ... and the other way round.
        not_found(&w.set_role(&b, b_org, alice, "admin").await);

        // Nothing changed anywhere.
        assert_eq!(w.role_in_db(b_org, bob).await.as_deref(), Some("owner"));
        assert_eq!(w.role_in_db(a_org, bob).await, None);
        assert_eq!(w.role_in_db(b_org, alice).await, None);
        assert_eq!(
            w.env
                .count("SELECT count(*) FROM organization_members WHERE role <> 'owner'")
                .await,
            0
        );
        let name: String = sqlx::query_scalar("SELECT name FROM organizations WHERE id = $1")
            .bind(b_org)
            .fetch_one(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(name, "Org org-b");

        // Own listing contains only own organizations.
        let mine = call(
            &w.env.app,
            Req::new("GET", "/api/v1/organizations").cookie_only(&a),
        )
        .await;
        let ids: Vec<_> = mine.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids, [a_org.to_string()]);
    }

    // ---- role rules -------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn plain_members_cannot_write_and_non_members_do_not_exist_for_them() {
        let w = World::new().await;
        let (owner, carol, eve) = (
            w.env.user("owner").await,
            w.env.user("carol").await,
            w.env.user("eve").await,
        );
        let org = w.org("acme", owner).await;
        let o = w.env.login_ok("owner").await;
        assert_eq!(
            w.add(&o, org, carol, "member").await.status,
            StatusCode::CREATED
        );
        let c = w.env.login_ok("carol").await;
        let e = w.env.login_ok("eve").await;

        // A member can read.
        let org_read = call(
            &w.env.app,
            Req::new("GET", &format!("/api/v1/organizations/{org}")).cookie_only(&c),
        )
        .await;
        assert_eq!(org_read.status, StatusCode::OK);
        assert_eq!(org_read.body["role"], "member");
        assert_eq!(w.members(&c, org).await.status, StatusCode::OK);

        // ... but not write (403, whether or not the target exists).
        let patch = call(
            &w.env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}"))
                .session(&c)
                .if_match("\"org-v1\"")
                .json(json!({"name": "Mine now"})),
        )
        .await;
        forbidden(&patch);
        forbidden(&w.add(&c, org, eve, "member").await);
        forbidden(&w.add(&c, org, Uuid::new_v4(), "member").await); // no user enumeration
        forbidden(&w.set_role(&c, org, owner, "member").await);
        forbidden(&w.set_role(&c, org, carol, "admin").await);
        forbidden(&w.set_role(&c, org, Uuid::new_v4(), "admin").await); // unknown target: still 403
        forbidden(&w.remove(&c, org, owner).await);
        forbidden(&w.remove(&c, org, Uuid::new_v4()).await);

        // A non-member gets 404 everywhere, including for the same unknown user.
        not_found(&w.add(&e, org, Uuid::new_v4(), "member").await);
        not_found(&w.members(&e, org).await);
        assert_eq!(w.role_in_db(org, owner).await.as_deref(), Some("owner"));
        assert_eq!(w.role_in_db(org, carol).await.as_deref(), Some("member"));

        // Members may leave; afterwards the organization is gone for them.
        assert_eq!(
            w.remove(&c, org, carol).await.status,
            StatusCode::NO_CONTENT
        );
        not_found(
            &call(
                &w.env.app,
                Req::new("GET", &format!("/api/v1/organizations/{org}")).cookie_only(&c),
            )
            .await,
        );
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn admins_manage_members_and_admins_but_never_touch_owners() {
        let w = World::new().await;
        let (owner, dave, carol, erin, frank) = (
            w.env.user("owner").await,
            w.env.user("dave").await,
            w.env.user("carol").await,
            w.env.user("erin").await,
            w.env.user("frank").await,
        );
        let org = w.org("acme", owner).await;
        let o = w.env.login_ok("owner").await;
        assert_eq!(
            w.add(&o, org, dave, "admin").await.status,
            StatusCode::CREATED
        );
        let d = w.env.login_ok("dave").await;

        // Admin adds members and admins; never an owner.
        assert_eq!(
            w.add(&d, org, carol, "member").await.status,
            StatusCode::CREATED
        );
        assert_eq!(
            w.add(&d, org, erin, "admin").await.status,
            StatusCode::CREATED
        );
        forbidden(&w.add(&d, org, frank, "owner").await);
        assert_eq!(w.role_in_db(org, frank).await, None);
        // Duplicate, unknown (404 only after authz), disabled and invalid input.
        assert_eq!(
            w.add(&d, org, carol, "member").await.code(),
            "ALREADY_MEMBER"
        );
        not_found(&w.add(&d, org, Uuid::new_v4(), "member").await);
        sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
            .bind(frank)
            .execute(&w.env.pool)
            .await
            .unwrap();
        assert_eq!(
            w.add(&d, org, frank, "member").await.code(),
            "ACCOUNT_DISABLED"
        );
        for bad in [
            json!({"user_id": "x", "role": "member"}),
            json!({"user_id": frank.to_string(), "role": "god"}),
            json!({"user_id": frank.to_string()}),
            json!({"user_id": frank.to_string(), "role": "member", "extra": 1}),
        ] {
            let r = call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/organizations/{org}/members"))
                    .session(&d)
                    .json(bad),
            )
            .await;
            assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", r.text);
        }

        // Role changes among members/admins work, promotion to owner does not.
        let promoted = w.set_role(&d, org, carol, "admin").await;
        assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.text);
        assert_eq!(promoted.body["role"], "admin");
        forbidden(&w.set_role(&d, org, carol, "owner").await);
        // Owners are untouchable for admins.
        forbidden(&w.set_role(&d, org, owner, "member").await);
        forbidden(&w.set_role(&d, org, owner, "admin").await);
        forbidden(&w.remove(&d, org, owner).await);
        assert_eq!(w.role_in_db(org, owner).await.as_deref(), Some("owner"));
        // Admins may manage other admins ...
        assert_eq!(
            w.set_role(&d, org, erin, "member").await.status,
            StatusCode::OK
        );
        assert_eq!(
            w.remove(&d, org, carol).await.status,
            StatusCode::NO_CONTENT
        );
        // ... unchanged role is a no-op success; an unknown member is 404 for managers.
        assert_eq!(
            w.set_role(&d, org, erin, "member").await.status,
            StatusCode::OK
        );
        not_found(&w.set_role(&d, org, carol, "member").await);
        not_found(&w.remove(&d, org, carol).await);
        // Self-demotion works; afterwards the former admin has no rights.
        assert_eq!(
            w.set_role(&d, org, dave, "member").await.status,
            StatusCode::OK
        );
        forbidden(&w.add(&d, org, frank, "member").await);
        // The change took effect on the next request (no stale role from a cache).
        let name_change = call(
            &w.env.app,
            Req::new("PATCH", &format!("/api/v1/organizations/{org}"))
                .session(&d)
                .if_match("\"org-v1\"")
                .json(json!({"name": "Late"})),
        )
        .await;
        forbidden(&name_change);
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn owners_assign_every_role_and_a_second_owner_makes_demotion_possible() {
        let w = World::new().await;
        let (x, y) = (w.env.user("xavier").await, w.env.user("yara").await);
        let org = w.org("acme", x).await;
        let sx = w.env.login_ok("xavier").await;
        assert_eq!(
            w.add(&sx, org, y, "owner").await.status,
            StatusCode::CREATED
        );
        assert_eq!(w.owners(org).await, 2);
        let sy = w.env.login_ok("yara").await;
        // Either owner may demote the other while two exist.
        assert_eq!(
            w.set_role(&sy, org, x, "member").await.status,
            StatusCode::OK
        );
        assert_eq!(w.owners(org).await, 1);
        // The demoted ex-owner is a plain member now.
        forbidden(&w.set_role(&sx, org, y, "member").await);
        // The last owner can neither demote nor remove themselves.
        let demote = w.set_role(&sy, org, y, "admin").await;
        assert_eq!(
            (demote.status, demote.code()),
            (StatusCode::CONFLICT, "LAST_OWNER")
        );
        let leave = w.remove(&sy, org, y).await;
        assert_eq!(
            (leave.status, leave.code()),
            (StatusCode::CONFLICT, "LAST_OWNER")
        );
        assert_eq!(w.role_in_db(org, y).await.as_deref(), Some("owner"));
        // Re-promoting the member to owner and then leaving is fine.
        assert_eq!(
            w.set_role(&sy, org, x, "owner").await.status,
            StatusCode::OK
        );
        assert_eq!(w.remove(&sy, org, y).await.status, StatusCode::NO_CONTENT);
        assert_eq!(w.owners(org).await, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn concurrent_demotions_and_departures_never_leave_an_organization_without_an_owner() {
        for round in 0..6 {
            let w = World::new().await;
            let (x, y) = (w.env.user("xavier").await, w.env.user("yara").await);
            let org = w.org("race", x).await;
            let sx = w.env.login_ok("xavier").await;
            assert_eq!(
                w.add(&sx, org, y, "owner").await.status,
                StatusCode::CREATED
            );
            let sy = w.env.login_ok("yara").await;

            let (a, b) = if round % 2 == 0 {
                // Both demote each other at the same moment.
                let (app1, app2) = (w.env.app.clone(), w.env.app.clone());
                let (sx1, sy1) = (sx.clone(), sy.clone());
                let t1 = tokio::spawn(async move {
                    call(
                        &app1,
                        Req::new("PATCH", &format!("/api/v1/organizations/{org}/members/{y}"))
                            .session(&sx1)
                            .json(json!({"role": "member"})),
                    )
                    .await
                    .status
                });
                let t2 = tokio::spawn(async move {
                    call(
                        &app2,
                        Req::new("PATCH", &format!("/api/v1/organizations/{org}/members/{x}"))
                            .session(&sy1)
                            .json(json!({"role": "member"})),
                    )
                    .await
                    .status
                });
                (t1.await.unwrap(), t2.await.unwrap())
            } else {
                // Both leave at the same moment.
                let (app1, app2) = (w.env.app.clone(), w.env.app.clone());
                let (sx1, sy1) = (sx.clone(), sy.clone());
                let t1 = tokio::spawn(async move {
                    call(
                        &app1,
                        Req::new(
                            "DELETE",
                            &format!("/api/v1/organizations/{org}/members/{x}"),
                        )
                        .session(&sx1),
                    )
                    .await
                    .status
                });
                let t2 = tokio::spawn(async move {
                    call(
                        &app2,
                        Req::new(
                            "DELETE",
                            &format!("/api/v1/organizations/{org}/members/{y}"),
                        )
                        .session(&sy1),
                    )
                    .await
                    .status
                });
                (t1.await.unwrap(), t2.await.unwrap())
            };
            let mut statuses = [a.as_u16(), b.as_u16()];
            statuses.sort_unstable();
            if round % 2 == 0 {
                // One demotion wins; the loser has lost the owner role (403) by the time it runs.
                assert_eq!(statuses, [200, 403], "round {round}");
            } else {
                assert_eq!(statuses, [204, 409], "round {round}");
            }
            assert_eq!(w.owners(org).await, 1, "round {round}");
        }
    }

    // ---- optimistic concurrency -------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn organization_updates_use_etag_and_if_match() {
        let w = World::new().await;
        let (owner, carol) = (w.env.user("owner").await, w.env.user("carol").await);
        let org = w.org("acme", owner).await;
        let o = w.env.login_ok("owner").await;
        w.add(&o, org, carol, "member").await;
        let c = w.env.login_ok("carol").await;
        let url = format!("/api/v1/organizations/{org}");

        let read = call(&w.env.app, Req::new("GET", &url).cookie_only(&o)).await;
        let etag = read.etag();
        assert_eq!(etag, "\"org-v1\"");

        // Authorization comes first: a member gets 403 even with the right ETag.
        forbidden(
            &call(
                &w.env.app,
                Req::new("PATCH", &url)
                    .session(&c)
                    .if_match(&etag)
                    .json(json!({"name": "X"})),
            )
            .await,
        );
        // Precondition required.
        let missing = call(
            &w.env.app,
            Req::new("PATCH", &url)
                .session(&o)
                .json(json!({"name": "New name"})),
        )
        .await;
        assert_eq!(
            (missing.status, missing.code()),
            (StatusCode::PRECONDITION_REQUIRED, "PRECONDITION_REQUIRED")
        );
        // Validation.
        let invalid = call(
            &w.env.app,
            Req::new("PATCH", &url)
                .session(&o)
                .if_match(&etag)
                .json(json!({"name": " "})),
        )
        .await;
        assert_eq!(invalid.body["error"]["details"]["fields"], json!(["name"]));
        // Wrong / malformed ETags are version conflicts and report the current one.
        for bad in ["\"org-v7\"", "W/\"org-v1\"", "*", "garbage"] {
            let r = call(
                &w.env.app,
                Req::new("PATCH", &url)
                    .session(&o)
                    .if_match(bad)
                    .json(json!({"name": "New name"})),
            )
            .await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::PRECONDITION_FAILED, "VERSION_CONFLICT"),
                "{bad}"
            );
            assert_eq!(r.etag(), "\"org-v1\"");
        }
        // Success increments the version; the old ETag is stale afterwards.
        let ok = call(
            &w.env.app,
            Req::new("PATCH", &url)
                .session(&o)
                .if_match(&etag)
                .json(json!({"name": "  New name  "})),
        )
        .await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
        assert_eq!(ok.body["name"], "New name");
        assert_eq!(ok.body["version"], 2);
        assert_eq!(ok.etag(), "\"org-v2\"");
        let stale = call(
            &w.env.app,
            Req::new("PATCH", &url)
                .session(&o)
                .if_match(&etag)
                .json(json!({"name": "Again"})),
        )
        .await;
        assert_eq!(stale.code(), "VERSION_CONFLICT");
        // Unknown fields are rejected.
        let extra = call(
            &w.env.app,
            Req::new("PATCH", &url)
                .session(&o)
                .if_match("\"org-v2\"")
                .json(json!({"name": "x", "slug": "y"})),
        )
        .await;
        assert_eq!(extra.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // ---- accounts ---------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabling_an_account_ends_its_access_and_enabling_restores_login() {
        let w = World::new().await;
        let (owner, carol) = (w.env.user("owner").await, w.env.user("carol").await);
        let org = w.org("acme", owner).await;
        let o = w.env.login_ok("owner").await;
        w.add(&o, org, carol, "member").await;
        let c1 = w.env.login_ok("carol").await;
        let c2 = w.env.login_ok("carol").await;
        let disable = |id: Uuid| {
            Req::new("POST", &format!("/api/v1/admin/users/{id}/disable")).session(&w.op)
        };
        let enable =
            |id: Uuid| Req::new("POST", &format!("/api/v1/admin/users/{id}/enable")).session(&w.op);

        // Only the operator; unknown ids 404; the operator cannot be disabled.
        forbidden(
            &call(
                &w.env.app,
                Req::new("POST", &format!("/api/v1/admin/users/{carol}/disable")).session(&o),
            )
            .await,
        );
        not_found(&call(&w.env.app, disable(Uuid::new_v4())).await);
        let op = call(&w.env.app, disable(w.env.operator_id)).await;
        assert_eq!(
            (op.status, op.code()),
            (StatusCode::CONFLICT, "CANNOT_DISABLE_OPERATOR")
        );
        assert_eq!(w.env.get_session(&w.op).await.status, StatusCode::OK);

        // A pending reset link must die with the account's access.
        let link = call(
            &w.env.app,
            Req::new(
                "POST",
                &format!("/api/v1/admin/users/{carol}/password-reset"),
            )
            .session(&w.op),
        )
        .await;
        assert_eq!(link.status, StatusCode::CREATED);
        let token = link.body["reset_url"]
            .as_str()
            .unwrap()
            .split("token=")
            .nth(1)
            .unwrap()
            .to_owned();

        let first = call(&w.env.app, disable(carol)).await;
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(first.body["orphaned_organization_ids"], json!([]));
        assert_eq!(
            call(&w.env.app, disable(carol)).await.status,
            StatusCode::OK,
            "idempotent"
        );
        assert_eq!(
            w.env.get_session(&c1).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            w.env.get_session(&c2).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(w.env.login("carol").await.status, StatusCode::UNAUTHORIZED);
        let reset = call(
            &w.env.app,
            Req::new("POST", "/api/v1/auth/password-reset")
                .json(json!({"token": token, "new_password": "a-fresh-password-1"})),
        )
        .await;
        assert_eq!(reset.code(), "INVALID_RESET_TOKEN");
        assert_eq!(w.env.count("SELECT count(*) FROM sessions WHERE revoked_at IS NULL AND user_id IN (SELECT id FROM users WHERE disabled_at IS NOT NULL)").await, 0);
        // Her membership stays (she can come back), but a disabled account cannot be added anew.
        assert_eq!(w.role_in_db(org, carol).await.as_deref(), Some("member"));

        assert_eq!(
            call(&w.env.app, enable(carol)).await.status,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(&w.env.app, enable(carol)).await.status,
            StatusCode::NO_CONTENT,
            "idempotent"
        );
        assert_eq!(w.env.login("carol").await.status, StatusCode::OK);
        // Old sessions stay revoked.
        assert_eq!(
            w.env.get_session(&c1).await.status,
            StatusCode::UNAUTHORIZED
        );
    }

    // ---- authentication, CSRF, expiry -------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn every_unsafe_route_needs_a_session_the_csrf_header_and_the_origin() {
        let w = World::new().await;
        let (owner, carol) = (w.env.user("owner").await, w.env.user("carol").await);
        let org = w.org("acme", owner).await;
        let o = w.env.login_ok("owner").await;
        w.add(&o, org, carol, "member").await;

        let routes: Vec<(&'static str, String, Option<serde_json::Value>)> = vec![
            (
                "POST",
                "/api/v1/organizations".to_owned(),
                Some(json!({"name": "N", "slug": "n", "owner_user_id": owner.to_string()})),
            ),
            (
                "PATCH",
                format!("/api/v1/organizations/{org}"),
                Some(json!({"name": "N"})),
            ),
            (
                "POST",
                format!("/api/v1/organizations/{org}/members"),
                Some(json!({"user_id": carol.to_string(), "role": "member"})),
            ),
            (
                "PATCH",
                format!("/api/v1/organizations/{org}/members/{carol}"),
                Some(json!({"role": "admin"})),
            ),
            (
                "DELETE",
                format!("/api/v1/organizations/{org}/members/{carol}"),
                None,
            ),
            ("POST", format!("/api/v1/admin/users/{carol}/disable"), None),
            ("POST", format!("/api/v1/admin/users/{carol}/enable"), None),
            ("GET", "/api/v1/organizations".to_owned(), None),
            ("GET", format!("/api/v1/organizations/{org}"), None),
            ("GET", format!("/api/v1/organizations/{org}/members"), None),
            ("GET", "/api/v1/admin/organizations".to_owned(), None),
        ];
        let build = |method: &'static str, uri: &str, body: &Option<serde_json::Value>| {
            let req = Req::new(method, uri);
            match body {
                Some(body) => req.json(body.clone()),
                None => req,
            }
        };
        for (method, uri, body) in &routes {
            // Anonymous: 401.
            let anon = call(&w.env.app, build(method, uri, body)).await;
            assert_eq!(
                (anon.status, anon.code()),
                (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
                "{method} {uri}"
            );
            if *method != "GET" {
                // Cookie without CSRF header: 403 CSRF_FAILED (before any policy decision).
                let no_csrf = call(&w.env.app, build(method, uri, body).cookie_only(&w.op)).await;
                assert_eq!(
                    (no_csrf.status, no_csrf.code()),
                    (StatusCode::FORBIDDEN, "CSRF_FAILED"),
                    "{method} {uri}"
                );
                // Right header, foreign Origin: 403 CSRF_FAILED.
                let bad_origin = call(
                    &w.env.app,
                    build(method, uri, body)
                        .session(&w.op)
                        .origin(Some("https://evil.example")),
                )
                .await;
                assert_eq!(bad_origin.code(), "CSRF_FAILED", "{method} {uri}");
            }
        }
        // Nothing happened.
        assert_eq!(w.env.count("SELECT count(*) FROM organizations").await, 2);
        assert_eq!(w.role_in_db(org, carol).await.as_deref(), Some("member"));
        assert_eq!(
            w.env
                .count("SELECT count(*) FROM users WHERE disabled_at IS NOT NULL")
                .await,
            0
        );

        // Expired sessions (8 h idle) are rejected on all routes.
        w.env.clock.advance(Duration::from_secs(8 * 3600 + 60));
        for (method, uri, body) in &routes {
            let r = call(&w.env.app, build(method, uri, body).session(&w.op)).await;
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::UNAUTHORIZED, "UNAUTHENTICATED"),
                "{method} {uri}"
            );
        }
    }

    // ---- reset links through the policy ------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn reset_links_are_an_operator_policy_action() {
        let w = World::new().await;
        let (owner, carol) = (w.env.user("owner").await, w.env.user("carol").await);
        let org = w.org("acme", owner).await;
        let o = w.env.login_ok("owner").await;
        // An organization owner is not an instance operator.
        forbidden(
            &call(
                &w.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/admin/users/{carol}/password-reset"),
                )
                .session(&o),
            )
            .await,
        );
        forbidden(
            &call(
                &w.env.app,
                Req::new(
                    "DELETE",
                    &format!("/api/v1/admin/users/{carol}/password-reset"),
                )
                .session(&o),
            )
            .await,
        );
        assert_eq!(
            call(
                &w.env.app,
                Req::new(
                    "POST",
                    &format!("/api/v1/admin/users/{carol}/password-reset")
                )
                .session(&w.op)
            )
            .await
            .status,
            StatusCode::CREATED
        );
        let _ = org;
    }

    // ---- audit events -------------------------------------------------------------

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn permission_decisions_and_changes_are_logged_as_audit_events_without_personal_data() {
        let logs = capture_logs();
        let w = World::new().await;
        let (owner, carol) = (
            w.env.user("auditowner").await,
            w.env.user("auditcarol").await,
        );
        w.env.user("auditeve").await;
        let org = w.org("audited", owner).await;
        let o = w.env.login_ok("auditowner").await;
        assert_eq!(
            w.add(&o, org, carol, "member").await.status,
            StatusCode::CREATED
        );
        let c = w.env.login_ok("auditcarol").await;
        let e = w.env.login_ok("auditeve").await;
        forbidden(&w.set_role(&c, org, owner, "member").await);
        not_found(&w.members(&e, org).await);
        assert_eq!(
            w.set_role(&o, org, carol, "admin").await.status,
            StatusCode::OK
        );

        let text = logs.text();
        for needle in [
            "craftcloud::audit",
            "member.add",
            "member.change_role",
            "organization.create",
            "success",
            "forbidden",
            "hidden",
            "request_id=",
            &owner.to_string(),
            &org.to_string(),
            &carol.to_string(),
        ] {
            assert!(text.contains(needle), "audit log lacks {needle}: {text}");
        }
        // No e-mail addresses, display names or secrets in the audit trail.
        let audit_lines: Vec<_> = text
            .lines()
            .filter(|l| l.contains("craftcloud::audit"))
            .collect();
        assert!(!audit_lines.is_empty());
        for line in audit_lines {
            for secret in [
                "@example.test",
                "auditowner",
                "auditcarol",
                PASSWORD,
                COOKIE_NAME,
                &o.token,
                &o.csrf,
            ] {
                assert!(!line.contains(secret), "audit line leaks {secret}: {line}");
            }
        }
        let _ = SKIP;
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn disabled_owners_do_not_count_and_disabling_the_last_owner_reports_the_orphaned_organization()
     {
        let logs = capture_logs();
        let w = World::new().await;
        let (x, y) = (w.env.user("xavier").await, w.env.user("yara").await);
        let org = w.org("acme", x).await;
        let sx = w.env.login_ok("xavier").await;
        assert_eq!(
            w.add(&sx, org, y, "owner").await.status,
            StatusCode::CREATED
        );
        let disable = |id: Uuid| {
            Req::new("POST", &format!("/api/v1/admin/users/{id}/disable")).session(&w.op)
        };

        // Disabling one of two owners orphans nothing.
        let first = call(&w.env.app, disable(y)).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.text);
        assert_eq!(first.body["orphaned_organization_ids"], json!([]));
        // The disabled co-owner does not count: the remaining owner cannot leave or step down.
        for r in [
            w.set_role(&sx, org, x, "admin").await,
            w.remove(&sx, org, x).await,
        ] {
            assert_eq!(
                (r.status, r.code()),
                (StatusCode::CONFLICT, "LAST_OWNER"),
                "{}",
                r.text
            );
        }
        assert_eq!(w.role_in_db(org, x).await.as_deref(), Some("owner"));

        // Disabling the last enabled owner is not blocked; the response lists the organization.
        let second = call(&w.env.app, disable(x)).await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.text);
        assert_eq!(
            second.body["orphaned_organization_ids"],
            json!([org.to_string()])
        );
        assert!(
            logs.text().contains("without an enabled owner"),
            "warning missing"
        );
        assert!(logs.text().contains("organization.orphaned"));
        // Idempotent re-disable still reports the orphan.
        let again = call(&w.env.app, disable(x)).await;
        assert_eq!(
            again.body["orphaned_organization_ids"],
            json!([org.to_string()])
        );
        // Her sessions are gone; the operator still has no access to the organization.
        assert_eq!(
            w.env.get_session(&sx).await.status,
            StatusCode::UNAUTHORIZED
        );
        not_found(
            &call(
                &w.env.app,
                Req::new("GET", &format!("/api/v1/organizations/{org}")).cookie_only(&w.op),
            )
            .await,
        );

        // Enabling restores the owner.
        let enable = call(
            &w.env.app,
            Req::new("POST", &format!("/api/v1/admin/users/{x}/enable")).session(&w.op),
        )
        .await;
        assert_eq!(enable.status, StatusCode::NO_CONTENT);
        let sx2 = w.env.login_ok("xavier").await;
        assert_eq!(
            call(
                &w.env.app,
                Req::new("GET", &format!("/api/v1/organizations/{org}")).cookie_only(&sx2)
            )
            .await
            .status,
            StatusCode::OK
        );
    }
}
