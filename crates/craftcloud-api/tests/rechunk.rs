//! PostgreSQL integration coverage for the offline storage rechunker.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::StatusCode;
    use craftcloud_storage::LocalStore;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use crate::common::{Req, TestEnv, call};

    fn sha_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn id(resp: &crate::common::Resp) -> Uuid {
        Uuid::parse_str(resp.body["id"].as_str().unwrap()).unwrap()
    }

    async fn create_file(
        env: &TestEnv,
        actor: &crate::common::Session,
        project: Uuid,
        name: &str,
    ) -> Uuid {
        let response = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project}/files"))
                .session(actor)
                .json(json!({"name":name, "app":"figcraft", "format":"fig"})),
        )
        .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text);
        id(&response)
    }

    async fn upload_version(
        env: &TestEnv,
        actor: &crate::common::Session,
        file: Uuid,
        data: &[u8],
        head: Option<Uuid>,
    ) -> Uuid {
        let created = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file}/uploads"))
                .session(actor)
                .json(json!({"size_bytes": data.len(), "sha256": sha_hex(data)})),
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.text);
        let upload = Uuid::parse_str(created.body["upload_id"].as_str().unwrap()).unwrap();
        let put = call(
            &env.app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload}/content"))
                .session(actor)
                .body(Body::from(data.to_vec())),
        )
        .await;
        assert_eq!(put.status, StatusCode::OK, "{}", put.text);
        let mut request = Req::new("POST", &format!("/api/v1/files/{file}/versions"))
            .session(actor)
            .json(json!({"upload_id": upload.to_string()}));
        request = match head {
            Some(head) => request.header("If-Match", &format!("\"version:{head}\"")),
            None => request.header("If-None-Match", "*"),
        };
        let committed = call(&env.app, request).await;
        assert_eq!(committed.status, StatusCode::CREATED, "{}", committed.text);
        id(&committed)
    }

    #[tokio::test]
    #[ignore = "requires CC_TEST_DATABASE_URL; run with cargo xtask db-test"]
    async fn rechunk_converts_all_versions_idempotently_and_preserves_quota_and_bytes() {
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        let owner_id = env.user("rechunk-owner").await;
        let owner = env.login_ok("rechunk-owner").await;
        let org_resp = call(&env.app, Req::new("POST", "/api/v1/organizations").session(&op)
            .json(json!({"name":"Rechunk", "slug":"rechunk-test", "owner_user_id":owner_id.to_string()}))).await;
        assert_eq!(org_resp.status, StatusCode::CREATED, "{}", org_resp.text);
        let org = id(&org_resp);
        let ws_resp = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"Workspace", "slug":"rechunk"})),
        )
        .await;
        assert_eq!(ws_resp.status, StatusCode::CREATED, "{}", ws_resp.text);
        let ws = id(&ws_resp);
        let project_resp = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/workspaces/{ws}/projects"))
                .session(&owner)
                .json(json!({"name":"Project"})),
        )
        .await;
        assert_eq!(
            project_resp.status,
            StatusCode::CREATED,
            "{}",
            project_resp.text
        );
        let project = id(&project_resp);
        let file_resp = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project}/files"))
                .session(&owner)
                .json(json!({"name":"source.fig", "app":"figcraft", "format":"fig"})),
        )
        .await;
        assert_eq!(file_resp.status, StatusCode::CREATED, "{}", file_resp.text);
        let file = id(&file_resp);

        let first = vec![0x31; 768 * 1024];
        let second = vec![0x72; 768 * 1024];
        let v1 = upload_version(&env, &owner, file, &first, None).await;
        let v2 = upload_version(&env, &owner, file, &second, Some(v1)).await;
        let quota_before: i64 =
            sqlx::query_scalar("SELECT committed_bytes FROM organizations WHERE id=$1")
                .bind(org)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let local = LocalStore::open(&env.storage_dir).await.unwrap();
        let keys = [craftcloud_storage::derive_chunk_key(&[0x42; 32]).unwrap()];

        let preview = craftcloud_api::rechunk::run(&env.pool, &local, Some(file), true, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (preview.whole_versions, preview.already_chunked_versions),
            (2, 0)
        );
        let still_whole: i64 = sqlx::query_scalar("SELECT count(*) FROM file_versions v JOIN blobs b ON b.id=v.blob_id WHERE v.file_id=$1 AND b.layout='whole'")
            .bind(file).fetch_one(&env.pool).await.unwrap();
        assert_eq!(still_whole, 2);

        let first_run =
            craftcloud_api::rechunk::run(&env.pool, &local, Some(file), false, &keys, None)
                .await
                .unwrap();
        assert_eq!(
            (first_run.converted_blobs, first_run.converted_versions),
            (2, 2)
        );
        let second_run =
            craftcloud_api::rechunk::run(&env.pool, &local, Some(file), false, &keys, None)
                .await
                .unwrap();
        assert_eq!(
            (second_run.converted_blobs, second_run.converted_versions),
            (0, 0)
        );
        let chunked: i64 = sqlx::query_scalar("SELECT count(*) FROM file_versions v JOIN blobs b ON b.id=v.blob_id WHERE v.file_id=$1 AND b.layout='chunked'")
            .bind(file).fetch_one(&env.pool).await.unwrap();
        assert_eq!(chunked, 2);
        let quota_after: i64 =
            sqlx::query_scalar("SELECT committed_bytes FROM organizations WHERE id=$1")
                .bind(org)
                .fetch_one(&env.pool)
                .await
                .unwrap();
        assert_eq!(quota_after, quota_before);
        for version in [v1, v2] {
            let got = call(
                &env.app,
                Req::new(
                    "GET",
                    &format!("/api/v1/files/{file}/versions/{version}/content"),
                )
                .session(&owner),
            )
            .await;
            assert_eq!(got.status, StatusCode::OK, "{}", got.text);
            let expected = if version == v1 { &first } else { &second };
            assert_eq!(Sha256::digest(got.bytes), Sha256::digest(expected));
        }
        env.pool.close().await;
    }

    #[tokio::test]
    #[ignore = "requires CC_TEST_DATABASE_URL; run with cargo xtask db-test"]
    async fn rechunk_all_skips_trashed_candidate_before_live_candidate() {
        let env = TestEnv::new().await;
        let op = env.login_ok("op").await;
        let owner_id = env.user("rechunk-trash-owner").await;
        let owner = env.login_ok("rechunk-trash-owner").await;
        let org_resp = call(
            &env.app,
            Req::new("POST", "/api/v1/organizations")
                .session(&op)
                .json(json!({"name":"Rechunk trash", "slug":"rechunk-trash", "owner_user_id":owner_id.to_string()})),
        )
        .await;
        assert_eq!(org_resp.status, StatusCode::CREATED, "{}", org_resp.text);
        let org = id(&org_resp);
        let ws_resp = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"Workspace", "slug":"rechunk-trash"})),
        )
        .await;
        assert_eq!(ws_resp.status, StatusCode::CREATED, "{}", ws_resp.text);
        let project_resp = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{}/projects", id(&ws_resp)),
            )
            .session(&owner)
            .json(json!({"name":"Project"})),
        )
        .await;
        assert_eq!(
            project_resp.status,
            StatusCode::CREATED,
            "{}",
            project_resp.text
        );
        let project = id(&project_resp);
        let first_file = create_file(&env, &owner, project, "first.fig").await;
        let second_file = create_file(&env, &owner, project, "second.fig").await;
        let payload = vec![0x5a; 128 * 1024];
        let first_version = upload_version(&env, &owner, first_file, &payload, None).await;
        let second_version = upload_version(&env, &owner, second_file, &payload, None).await;
        let rows: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
            "SELECT v.file_id,v.id,b.id FROM file_versions v JOIN blobs b ON b.id=v.blob_id WHERE v.id=ANY($1::uuid[]) ORDER BY b.id",
        )
        .bind([first_version, second_version])
        .fetch_all(&env.pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        let (trashed_file, _, trashed_blob) = rows[0];
        let (live_file, live_version, live_blob) = rows[1];
        assert!(
            trashed_blob < live_blob,
            "fixture must place trash first in blob keyset order"
        );
        sqlx::query("UPDATE files SET deleted_at=now(),deleted_by_user_id=$2 WHERE id=$1")
            .bind(trashed_file)
            .bind(owner_id)
            .execute(&env.pool)
            .await
            .unwrap();

        let inventory = craftcloud_db::rechunk::inventory(&env.pool, None)
            .await
            .unwrap();
        assert_eq!(inventory, (1, 0, payload.len() as i64));
        let candidates = craftcloud_db::rechunk::candidates(&env.pool, None, None, 32)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].file_id, live_file);
        assert_eq!(candidates[0].blob_id, live_blob);
        let local = LocalStore::open(&env.storage_dir).await.unwrap();
        let preview = craftcloud_api::rechunk::run(&env.pool, &local, None, true, &[], None)
            .await
            .unwrap();
        assert_eq!(
            (preview.whole_versions, preview.logical_bytes),
            (1, payload.len() as u64)
        );
        let keys = [craftcloud_storage::derive_chunk_key(&[0x42; 32]).unwrap()];
        let applied = craftcloud_api::rechunk::run(&env.pool, &local, None, false, &keys, None)
            .await
            .unwrap();
        assert_eq!(
            (applied.converted_blobs, applied.converted_versions),
            (1, 1)
        );
        let repeated = craftcloud_api::rechunk::run(&env.pool, &local, None, false, &keys, None)
            .await
            .unwrap();
        assert_eq!(
            (repeated.converted_blobs, repeated.converted_versions),
            (0, 0)
        );
        let trashed_layout: String = sqlx::query_scalar(
            "SELECT b.layout FROM file_versions v JOIN blobs b ON b.id=v.blob_id WHERE v.id=$1",
        )
        .bind(rows[0].1)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        let live_layout: String = sqlx::query_scalar(
            "SELECT b.layout FROM file_versions v JOIN blobs b ON b.id=v.blob_id WHERE v.id=$1",
        )
        .bind(live_version)
        .fetch_one(&env.pool)
        .await
        .unwrap();
        assert_eq!(trashed_layout, "whole");
        assert_eq!(live_layout, "chunked");
        env.pool.close().await;
    }
}
