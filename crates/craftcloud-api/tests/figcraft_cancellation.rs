//! PostgreSQL integration test for FigCraft operations after HTTP cancellation.

#[cfg(test)]
mod common;

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
        time::Duration,
    };

    use axum::http::StatusCode;
    use craftcloud_figcraft::{Access, Backend, Error, Export, Link};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use crate::common::{Req, Session, TestEnv, call};

    #[derive(Default)]
    struct SlowLinkBackend {
        next_link: AtomicUsize,
        create_calls: AtomicUsize,
        block_next: AtomicBool,
        entered: AtomicBool,
        released: AtomicBool,
    }

    impl Backend for SlowLinkBackend {
        fn publish(&self, _original: &[u8], _name: &str) -> Result<String, Error> {
            Ok("cancellation-test-file".to_owned())
        }

        fn create_link(&self, _file: &str, _access: Access) -> Result<Link, Error> {
            self.create_calls.fetch_add(1, Ordering::SeqCst);
            if self.block_next.swap(false, Ordering::SeqCst) {
                self.entered.store(true, Ordering::SeqCst);
                while !self.released.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            let id = self.next_link.fetch_add(1, Ordering::SeqCst);
            Ok(Link {
                id: format!("cancellation-link-{id}"),
                token: format!("cancellation-token-{id}"),
            })
        }

        fn revoke_link(&self, _file: &str, _link: &str) -> Result<(), Error> {
            Ok(())
        }

        fn export(&self, _file: &str, original: &[u8]) -> Result<Export, Error> {
            Ok(Export {
                bytes: original.to_vec(),
            })
        }

        fn remove(&self, _file: &str) -> Result<(), Error> {
            Ok(())
        }
    }

    struct Fixture {
        env: TestEnv,
        owner: Session,
        file: Uuid,
        backend: Arc<SlowLinkBackend>,
    }

    fn document() -> Vec<u8> {
        let doc = figcraft_doc::Document::new("HTTP cancellation");
        figcraft_format::save_bytes_with_history(&doc, None, &[], &[]).unwrap()
    }

    fn digest(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    async fn fixture() -> Fixture {
        let mut env = TestEnv::new().await;
        let backend = Arc::new(SlowLinkBackend::default());
        backend.block_next.store(true, Ordering::SeqCst);
        env.configure_figcraft(backend.clone(), "ws://figcraft.test/figcraft")
            .await;
        let owner = env.login_ok("op").await;
        let org: Uuid =
            sqlx::query_scalar("SELECT id FROM organizations WHERE slug='operator-org'")
                .fetch_one(&env.pool)
                .await
                .unwrap();
        let workspace = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/organizations/{org}/workspaces"))
                .session(&owner)
                .json(json!({"name":"Cancellation","slug":"cancel-live"})),
        )
        .await;
        assert_eq!(workspace.status, StatusCode::CREATED, "{}", workspace.text);
        let workspace_id = Uuid::parse_str(workspace.body["id"].as_str().unwrap()).unwrap();
        let project = call(
            &env.app,
            Req::new(
                "POST",
                &format!("/api/v1/workspaces/{workspace_id}/projects"),
            )
            .session(&owner)
            .json(json!({"name":"Live"})),
        )
        .await;
        assert_eq!(project.status, StatusCode::CREATED, "{}", project.text);
        let project_id = Uuid::parse_str(project.body["id"].as_str().unwrap()).unwrap();
        let file = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/projects/{project_id}/files"))
                .session(&owner)
                .json(json!({"name":"cancel.figcraft","app":"figcraft","format":"fig"})),
        )
        .await;
        assert_eq!(file.status, StatusCode::CREATED, "{}", file.text);
        let file_id = Uuid::parse_str(file.body["id"].as_str().unwrap()).unwrap();
        let bytes = document();
        let upload = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/uploads"))
                .session(&owner)
                .json(json!({"size_bytes":bytes.len(),"sha256":digest(&bytes)})),
        )
        .await;
        assert_eq!(upload.status, StatusCode::CREATED, "{}", upload.text);
        let upload_id = upload.body["upload_id"].as_str().unwrap();
        let stored = call(
            &env.app,
            Req::new("PUT", &format!("/api/v1/uploads/{upload_id}/content"))
                .session(&owner)
                .body(axum::body::Body::from(bytes)),
        )
        .await;
        assert_eq!(stored.status, StatusCode::OK, "{}", stored.text);
        let committed = call(
            &env.app,
            Req::new("POST", &format!("/api/v1/files/{file_id}/versions"))
                .session(&owner)
                .header("If-None-Match", "*")
                .json(json!({"upload_id":upload_id})),
        )
        .await;
        assert_eq!(committed.status, StatusCode::CREATED, "{}", committed.text);
        Fixture {
            env,
            owner,
            file: file_id,
            backend,
        }
    }

    #[tokio::test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    async fn cancelled_open_finishes_and_retry_returns_persisted_link() {
        let fixture = fixture().await;
        let app = fixture.env.app.clone();
        let session = fixture.owner.clone();
        let file = fixture.file;
        let request = tokio::spawn(async move {
            call(
                &app,
                Req::new("POST", &format!("/api/v1/files/{file}/live")).session(&session),
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(10), async {
            while !fixture.backend.entered.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("backend link request did not start");
        request.abort();
        let _ = request.await;
        fixture.backend.released.store(true, Ordering::SeqCst);

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let count: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM figcraft_links WHERE file_id=$1 AND revoked_at IS NULL",
                )
                .bind(fixture.file)
                .fetch_one(&fixture.env.pool)
                .await
                .unwrap();
                if count == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("detached FigCraft operation did not persist its link");

        let retry = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file))
                .session(&fixture.owner),
        )
        .await;
        assert_eq!(retry.status, StatusCode::OK, "{}", retry.text);
        assert!(
            retry.body["join_url"]
                .as_str()
                .unwrap()
                .contains("cancellation-token-0")
        );
        assert_eq!(fixture.backend.create_calls.load(Ordering::SeqCst), 1);

        fixture.env.services.begin_draining();
        let rejected = call(
            &fixture.env.app,
            Req::new("POST", &format!("/api/v1/files/{}/live", fixture.file))
                .session(&fixture.owner),
        )
        .await;
        assert_eq!(rejected.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(fixture.backend.create_calls.load(Ordering::SeqCst), 1);
    }
}
