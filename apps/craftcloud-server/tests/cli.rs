//! Black-box tests of the server binary: exit codes and database startup
//! behaviour. Tests marked `#[ignore]` need PostgreSQL (`cargo xtask db-test`).

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::process::{Command, Output, Stdio};
    use std::str::FromStr;
    use std::time::{Duration, Instant};

    use sqlx::postgres::PgConnectOptions;
    use sqlx::{Connection, PgConnection};

    const BIN: &str = env!("CARGO_BIN_EXE_craftcloud-server");

    fn command(args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(args);
        for (key, _) in std::env::vars() {
            if key.to_ascii_uppercase().starts_with("CRAFTCLOUD_") {
                cmd.env_remove(key);
            }
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd
    }

    fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
        command(args, env).output().unwrap()
    }

    fn text(output: &Output) -> String {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    #[test]
    fn unknown_arguments_exit_2_with_usage() {
        for args in [&["nope"][..], &["migrate", "extra"][..]] {
            let out = run(args, &[]);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
            assert!(text(&out).contains("usage: craftcloud-server"));
        }
        assert_eq!(run(&["--help"], &[]).status.code(), Some(0));
    }

    #[test]
    fn database_url_env_is_rejected_pointing_to_file_key() {
        let out = run(
            &[],
            &[("CRAFTCLOUD_DATABASE_URL", "postgres://u:hunter2@h/db")],
        );
        assert_eq!(out.status.code(), Some(2));
        let text = text(&out);
        assert!(text.contains("CRAFTCLOUD_DATABASE_URL_FILE"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn migrate_without_database_configuration_exits_2() {
        let out = run(&["migrate"], &[]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
    }

    #[test]
    fn bad_admin_dir_exits_2_before_anything_else_is_touched() {
        let missing = std::env::temp_dir().join("craftcloud-cli-test-no-admin-dir");
        let missing = missing.to_string_lossy().into_owned();
        // A database file that does not exist would also be exit 2; the message
        // proves that the admin directory is checked first.
        let nowhere = std::env::temp_dir().join("craftcloud-cli-test-no-db-file");
        let nowhere = nowhere.to_string_lossy().into_owned();
        let out = run(
            &[],
            &[
                ("CRAFTCLOUD_ADMIN_DIR", &missing),
                ("CRAFTCLOUD_DATABASE_URL_FILE", &nowhere),
            ],
        );
        assert_eq!(out.status.code(), Some(2));
        let first = text(&out);
        assert!(first.contains("CRAFTCLOUD_ADMIN_DIR"), "{first}");
        assert!(!first.contains("CRAFTCLOUD_DATABASE_URL_FILE"), "{first}");
        // A directory without index.html is refused the same way.
        let empty = std::env::temp_dir().join(format!(
            "craftcloud-cli-test-empty-admin-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&empty).unwrap();
        let empty_text = empty.to_string_lossy().into_owned();
        let out = run(&[], &[("CRAFTCLOUD_ADMIN_DIR", &empty_text)]);
        let _ = std::fs::remove_dir_all(&empty);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("no index.html"));
    }

    #[test]
    fn missing_secret_file_exits_2() {
        let path = std::env::temp_dir().join("craftcloud-cli-test-does-not-exist");
        let path = path.to_string_lossy().into_owned();
        let out = run(&[], &[("CRAFTCLOUD_DATABASE_URL_FILE", &path)]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
    }

    #[test]
    fn unusable_storage_directory_stops_startup_with_exit_1() {
        let missing = std::env::temp_dir().join("craftcloud-cli-test-storage-missing");
        let missing = missing.to_string_lossy().into_owned();
        let out = run(
            &[],
            &[
                ("CRAFTCLOUD_STORAGE_DIR", &missing),
                ("CRAFTCLOUD_BIND", "127.0.0.1:0"),
            ],
        );
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(text(&out).contains("storage directory"), "{}", text(&out));
        // A relative path is a configuration error.
        let out = run(&[], &[("CRAFTCLOUD_STORAGE_DIR", "relative")]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("CRAFTCLOUD_STORAGE_DIR"));
    }

    // ---- PostgreSQL-backed tests -------------------------------------

    /// Temp secret file plus throwaway database, both cleaned up on drop.
    struct Fixture {
        admin: PgConnectOptions,
        db_name: String,
        secret_path: PathBuf,
        password: String,
    }

    impl Fixture {
        fn new() -> Self {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let admin_url =
                std::env::var("CC_TEST_DATABASE_URL").expect("CC_TEST_DATABASE_URL must be set");
            let admin = PgConnectOptions::from_str(&admin_url).unwrap();
            let db_name = format!("cc_test_srv_{}_{}", std::process::id(), nanos());
            rt.block_on(async {
                let mut conn = PgConnection::connect_with(&admin).await.unwrap();
                sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
                    .execute(&mut conn)
                    .await
                    .unwrap();
            });
            let (base, _) = admin_url.rsplit_once('/').unwrap();
            let url = format!("{base}/{db_name}");
            let password = admin_url
                .split_once("://")
                .and_then(|(_, rest)| rest.split_once('@'))
                .and_then(|(userinfo, _)| userinfo.split_once(':'))
                .map(|(_, pw)| pw.to_owned())
                .unwrap();
            let secret_path = std::env::temp_dir().join(format!("craftcloud-cli-secret-{db_name}"));
            let mut file = std::fs::File::create(&secret_path).unwrap();
            writeln!(file, "{url}").unwrap();
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            Self {
                admin,
                db_name,
                secret_path,
                password,
            }
        }

        fn secret(&self) -> String {
            self.secret_path.to_string_lossy().into_owned()
        }

        fn assert_no_password(&self, output: &Output) {
            assert!(
                !text(output).contains(&self.password),
                "password leaked into output"
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.secret_path);
            let admin = self.admin.clone();
            let name = self.db_name.clone();
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                        let _ = sqlx::query(&format!(
                            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                        ))
                        .execute(&mut conn)
                        .await;
                    }
                });
            }
        }
    }

    /// Unique per call within the process (the clock alone can repeat).
    fn nanos() -> String {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{time}_{n}")
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn http_get(port: u16, path: &str) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        Some(response)
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn unmigrated_database_blocks_startup_then_migrate_then_serves() {
        let fx = Fixture::new();
        let secret = fx.secret();
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let env = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", secret.as_str()),
            ("CRAFTCLOUD_BIND", bind.as_str()),
        ];

        // 1. Unmigrated: refuse to start, point at the migrate command.
        let out = run(&[], &env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(
            text(&out).contains("craftcloud-server migrate"),
            "{}",
            text(&out)
        );
        fx.assert_no_password(&out);
        assert!(
            http_get(port, "/health/live").is_none(),
            "must not have bound"
        );

        // 2. migrate applies both versions, second run applies none.
        let out = run(&["migrate"], &env);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        assert!(text(&out).contains("applied migration 1"));
        assert!(text(&out).contains("applied migration 2"));
        assert!(text(&out).contains("applied migration 3"));
        assert!(text(&out).contains("applied migration 4"));
        assert!(text(&out).contains("applied migration 5"));
        assert!(text(&out).contains("applied migration 9"));
        assert!(text(&out).contains("applied migration 10"));
        assert!(text(&out).contains("applied migration 12"));
        assert!(text(&out).contains("applied 12 migration(s)"));
        fx.assert_no_password(&out);
        let out = run(&["migrate"], &env);
        assert_eq!(out.status.code(), Some(0));
        assert!(
            text(&out).contains("no pending migrations"),
            "{}",
            text(&out)
        );

        // 3. Normal start now serves requests.
        let mut child = command(&[], &env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut served = None;
        while Instant::now() < deadline {
            if let Some(response) = http_get(port, "/health/live") {
                served = Some(response);
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        child.kill().unwrap();
        let output = child.wait_with_output().unwrap();
        let served = served.expect("server did not answer /health/live");
        assert!(served.starts_with("HTTP/1.1 200"), "{served}");
        fx.assert_no_password(&output);
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn wrong_password_fails_without_leaking_credentials() {
        let fx = Fixture::new();
        // Overwrite the secret with a URL using a wrong password.
        let bad = std::fs::read_to_string(&fx.secret_path)
            .unwrap()
            .replace(&fx.password, "wrong-secret-xyz");
        std::fs::write(&fx.secret_path, bad).unwrap();
        let secret = fx.secret();
        let out = run(&["migrate"], &[("CRAFTCLOUD_DATABASE_URL_FILE", &secret)]);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(!text(&out).contains("wrong-secret-xyz"));
        fx.assert_no_password(&out);
    }

    // ---- bootstrap secret --------------------------------------------

    const BOOTSTRAP_TOKEN: &str = "cli-test-bootstrap-token-0123456789abcdef";

    /// Temp secret file removed on drop.
    struct TempSecret(PathBuf);

    impl TempSecret {
        fn new(tag: &str, content: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "craftcloud-cli-{tag}-{}-{}",
                std::process::id(),
                nanos()
            ));
            std::fs::write(&path, content).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            Self(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempSecret {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn bootstrap_secret_without_database_exits_2() {
        let secret = TempSecret::new("boot", BOOTSTRAP_TOKEN);
        let out = run(&[], &[("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE", &secret.path())]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
        assert!(!text(&out).contains(BOOTSTRAP_TOKEN));
    }

    #[test]
    fn short_or_missing_bootstrap_token_exits_2_before_binding() {
        let db_file = TempSecret::new("dburl", "postgres://u:p@127.0.0.1:1/none");
        let short = TempSecret::new("short", "too-short-token");
        let out = run(
            &[],
            &[
                ("CRAFTCLOUD_DATABASE_URL_FILE", &db_file.path()),
                ("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE", &short.path()),
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(
            text(&out).contains("at least 32 characters"),
            "{}",
            text(&out)
        );
        assert!(!text(&out).contains("too-short-token"));

        let missing = std::env::temp_dir().join("craftcloud-cli-no-such-bootstrap");
        let out = run(
            &[],
            &[
                ("CRAFTCLOUD_DATABASE_URL_FILE", &db_file.path()),
                (
                    "CRAFTCLOUD_BOOTSTRAP_SECRET_FILE",
                    &missing.to_string_lossy(),
                ),
            ],
        );
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE"));
    }

    fn http_get_bearer(port: u16, path: &str, token: Option<&str>) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let auth = token.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n"
        )
        .ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        Some(response)
    }

    const METRICS_TOKEN: &str = "cli-metrics-token-0123456789-abcdefghij";

    #[test]
    fn metrics_listener_settings_are_validated_before_anything_starts() {
        let token = TempSecret::new("metrics", METRICS_TOKEN);
        // Not loopback and no token: refused (exit 2) without echoing anything secret.
        let out = run(&[], &[("CRAFTCLOUD_METRICS_ADDR", "0.0.0.0:9464")]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(
            text(&out).contains("CRAFTCLOUD_METRICS_ADDR"),
            "{}",
            text(&out)
        );
        // A token without a listener has no effect and is refused.
        let out = run(&[], &[("CRAFTCLOUD_METRICS_TOKEN_FILE", &token.path())]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        // A too short token is refused before the socket is bound.
        let short = TempSecret::new("metrics-short", "short");
        let out = run(
            &[],
            &[
                ("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:0"),
                ("CRAFTCLOUD_METRICS_TOKEN_FILE", &short.path()),
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("CRAFTCLOUD_METRICS_TOKEN_FILE"));
        assert!(!text(&out).contains(
            "short
"
        ));
        // A missing token file likewise.
        let missing = std::env::temp_dir().join("craftcloud-cli-no-such-metrics-token");
        let out = run(
            &[],
            &[
                ("CRAFTCLOUD_METRICS_ADDR", "127.0.0.1:0"),
                ("CRAFTCLOUD_METRICS_TOKEN_FILE", &missing.to_string_lossy()),
            ],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    }

    #[test]
    fn metrics_are_served_on_a_separate_protected_listener_and_requests_are_logged_as_json() {
        let token = TempSecret::new(
            "metrics-ok",
            &format!(
                "{METRICS_TOKEN}
"
            ),
        );
        let port = free_port();
        let metrics_port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let metrics_addr = format!("127.0.0.1:{metrics_port}");
        let env = [
            ("CRAFTCLOUD_BIND", bind.as_str()),
            ("CRAFTCLOUD_METRICS_ADDR", metrics_addr.as_str()),
            ("CRAFTCLOUD_METRICS_TOKEN_FILE", &token.path()),
            ("CRAFTCLOUD_LOG_FORMAT", "json"),
        ];
        let mut child = command(&[], &env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(wait_for_server(port), "server did not start");

        // Public listener: no metrics; without a database it is not ready.
        let public = http_get(port, "/metrics").unwrap();
        assert!(public.starts_with("HTTP/1.1 404"), "{public}");
        let ready = http_get(port, "/health/ready").unwrap();
        assert!(ready.starts_with("HTTP/1.1 503"), "{ready}");
        assert!(ready.contains("NOT_READY"), "{ready}");
        let info = http_get(port, "/api/v1/info?access_token=QUERYSECRET-31337").unwrap();
        assert!(info.starts_with("HTTP/1.1 200"), "{info}");

        // Metrics listener: the bearer token is required.
        let mut open = None;
        for _ in 0..50 {
            open = http_get_bearer(metrics_port, "/metrics", None);
            if open.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let open = open.expect("metrics listener did not start");
        assert!(open.starts_with("HTTP/1.1 401"), "{open}");
        let wrong = http_get_bearer(metrics_port, "/metrics", Some("wrong-wrong-wrong")).unwrap();
        assert!(wrong.starts_with("HTTP/1.1 401"), "{wrong}");
        let ok = http_get_bearer(metrics_port, "/metrics", Some(METRICS_TOKEN)).unwrap();
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("text/plain; version=0.0.4"), "{ok}");
        assert!(
            ok.contains(
                "craftcloud_http_requests_total{route=\"/api/v1/info\",method=\"GET\",status_class=\"2xx\"} 1"
            ),
            "{ok}"
        );
        assert!(!ok.contains("QUERYSECRET"), "{ok}");
        // The public listener has no route of that name on the metrics port either.
        let other = http_get_bearer(metrics_port, "/api/v1/info", Some(METRICS_TOKEN)).unwrap();
        assert!(other.starts_with("HTTP/1.1 404"), "{other}");

        child.kill().unwrap();
        let output = child.wait_with_output().unwrap();
        let log = text(&output);
        for secret in [METRICS_TOKEN, "QUERYSECRET"] {
            assert!(!log.contains(secret), "{secret} leaked into the log");
        }
        // Every line is one JSON object; the request line carries the route
        // template, status, latency and the pseudonymous ids.
        let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| serde_json::from_str(l).expect(l))
            .collect();
        let request = lines
            .iter()
            .find(|l| l["target"] == "craftcloud::request" && l["span"]["route"] == "/api/v1/info")
            .unwrap_or_else(|| panic!("no request line in {log}"));
        assert_eq!(request["fields"]["status"], 200);
        assert_eq!(request["span"]["method"], "GET");
        assert!(request["fields"]["latency_ms"].is_u64());
        assert_eq!(request["fields"]["actor_id"], "-");
        assert_eq!(request["fields"]["org_id"], "-");
        assert!(request["span"]["request_id"].as_str().is_some());
    }

    fn http_post(port: u16, path: &str, body: &str) -> Option<String> {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .ok()?;
        write!(
            stream,
            "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .ok()?;
        let mut response = String::new();
        stream.read_to_string(&mut response).ok()?;
        Some(response)
    }

    fn wait_for_server(port: u16) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if http_get(port, "/health/live").is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn setup_over_http_then_restart_warns_to_remove_the_secret() {
        let fx = Fixture::new();
        let db_secret = fx.secret();
        let bootstrap = TempSecret::new("bootstrap", &format!("{BOOTSTRAP_TOKEN}\n"));
        let boot_path = bootstrap.path();
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let env = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", db_secret.as_str()),
            ("CRAFTCLOUD_BOOTSTRAP_SECRET_FILE", boot_path.as_str()),
            ("CRAFTCLOUD_BIND", bind.as_str()),
            ("CRAFTCLOUD_LOG_FORMAT", "json"),
        ];
        assert_eq!(run(&["migrate"], &env).status.code(), Some(0));

        let password = "cli-test-operator-password-123";
        let request = format!(
            r#"{{"bootstrap_token":"{BOOTSTRAP_TOKEN}","email":"op@example.test","display_name":"Op","password":"{password}","organization":{{"name":"Org","slug":"org"}}}}"#
        );

        // First run: status required, wrong token 403, right token 201.
        let mut child = command(&[], &env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(wait_for_server(port), "server did not start");
        let status = http_get(port, "/api/v1/setup/status").unwrap();
        assert!(status.contains(r#""setup_required":true"#), "{status}");
        let wrong = request.replace(BOOTSTRAP_TOKEN, "wrong-token-wrong-token-wrong-token-xxxx");
        let denied = http_post(port, "/api/v1/setup/complete", &wrong).unwrap();
        assert!(denied.starts_with("HTTP/1.1 403"), "{denied}");
        let created = http_post(port, "/api/v1/setup/complete", &request).unwrap();
        assert!(created.starts_with("HTTP/1.1 201"), "{created}");
        let again = http_post(port, "/api/v1/setup/complete", &request).unwrap();
        assert!(again.starts_with("HTTP/1.1 409"), "{again}");
        child.kill().unwrap();
        let first = child.wait_with_output().unwrap();
        for secret in [BOOTSTRAP_TOKEN, password, "wrong-token-wrong"] {
            assert!(!text(&first).contains(secret), "{secret} leaked into logs");
        }
        fx.assert_no_password(&first);

        // Restart with the secret file still configured: warns, never prints it.
        let mut child = command(&[], &env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        assert!(wait_for_server(port), "server did not restart");
        let status = http_get(port, "/api/v1/setup/status").unwrap();
        assert!(status.contains(r#""setup_required":false"#), "{status}");
        child.kill().unwrap();
        let second = child.wait_with_output().unwrap();
        assert!(
            text(&second).contains("remove the bootstrap secret file"),
            "{}",
            text(&second)
        );
        assert!(!text(&second).contains(BOOTSTRAP_TOKEN));
    }

    // ---- admin issue-password-reset ------------------------------------

    #[test]
    fn issue_password_reset_requires_arguments_public_url_and_database() {
        // Missing --email / extra arguments: usage, exit 2.
        for args in [
            &["admin"][..],
            &["admin", "issue-password-reset"][..],
            &["admin", "issue-password-reset", "--email"][..],
            &["admin", "issue-password-reset", "--password", "x"][..],
            &["admin", "nope", "--email", "a@b.test"][..],
        ] {
            let out = run(args, &[]);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
            assert!(text(&out).contains("usage: craftcloud-server"));
        }
        let args = ["admin", "issue-password-reset", "--email", "a@b.test"];
        // No PUBLIC_URL: exit 2 before touching anything.
        let db_file = TempSecret::new("dburl", "postgres://u:p@127.0.0.1:1/none");
        let out = run(&args, &[("CRAFTCLOUD_DATABASE_URL_FILE", &db_file.path())]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("CRAFTCLOUD_PUBLIC_URL"));
        // No database: exit 2.
        let out = run(
            &args,
            &[("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org")],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
    }

    impl Fixture {
        fn exec(&self, sql: &str) {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let options = self.admin.clone().database(&self.db_name);
            rt.block_on(async {
                let mut conn = PgConnection::connect_with(&options).await.unwrap();
                sqlx::query(sql).execute(&mut conn).await.unwrap();
            });
        }

        fn scalar(&self, sql: &str) -> String {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let options = self.admin.clone().database(&self.db_name);
            rt.block_on(async {
                let mut conn = PgConnection::connect_with(&options).await.unwrap();
                sqlx::query_scalar::<_, String>(sql)
                    .fetch_one(&mut conn)
                    .await
                    .unwrap()
            })
        }
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn issue_password_reset_prints_a_single_use_link_to_stdout_only() {
        let fx = Fixture::new();
        let db_secret = fx.secret();
        let env = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", db_secret.as_str()),
            ("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org"),
            ("CRAFTCLOUD_LOG_FORMAT", "json"),
        ];
        let args = [
            "admin",
            "issue-password-reset",
            "--email",
            "Ria@Example.test",
        ];

        // Unmigrated database: refuse and point at migrate.
        let out = run(&args, &env);
        assert_eq!(out.status.code(), Some(1), "{}", text(&out));
        assert!(text(&out).contains("craftcloud-server migrate"));

        assert_eq!(run(&["migrate"], &env).status.code(), Some(0));
        fx.exec("INSERT INTO users (id, normalized_email, display_name) VALUES (gen_random_uuid(), 'ria@example.test', 'Ria')");

        // Unknown account: exit 1, nothing stored.
        let unknown = run(
            &[
                "admin",
                "issue-password-reset",
                "--email",
                "nobody@example.test",
            ],
            &env,
        );
        assert_eq!(unknown.status.code(), Some(1), "{}", text(&unknown));
        assert!(text(&unknown).contains("no account"));
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM password_reset_tokens"),
            "0"
        );

        let out = run(&args, &env);
        assert_eq!(out.status.code(), Some(0), "{}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let marker = "https://cloud.example.org/reset-password#token=";
        let line = stdout
            .lines()
            .find(|l| l.starts_with(marker))
            .expect(&stdout);
        let token = line.trim_start_matches(marker);
        assert_eq!(token.len(), 43);
        // The token appears exactly once, on stdout, and never in logs/stderr.
        assert_eq!(text(&out).matches(token).count(), 1);
        assert!(!stderr.contains(token));
        fx.assert_no_password(&out);
        // A warning next to the link says it is a secret.
        assert!(
            stderr.contains("WARNING") && stderr.contains("secret"),
            "{stderr}"
        );

        // Stored: only the SHA-256 hash, no issuer, expiry one hour out.
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM password_reset_tokens"),
            "1"
        );
        assert_eq!(
            fx.scalar("SELECT (created_by_user_id IS NULL)::text FROM password_reset_tokens"),
            "true"
        );
        // The offline issuance leaves one audit event without actor, and no token.
        assert_eq!(
            fx.scalar(
                "SELECT count(*)::text FROM audit_events                  WHERE action = 'password_reset.issue' AND actor_id IS NULL AND result = 'success'"
            ),
            "1"
        );
        assert_eq!(
            fx.scalar(&format!(
                "SELECT count(*)::text FROM audit_events a WHERE row_to_json(a)::text LIKE '%{token}%'"
            )),
            "0"
        );
        assert_eq!(
            fx.scalar("SELECT (expires_at - created_at = interval '1 hour')::text FROM password_reset_tokens"),
            "true"
        );
        let expected = fx.scalar(&format!(
            "SELECT encode(sha256(convert_to('{token}', 'UTF8')), 'hex')"
        ));
        assert_eq!(
            fx.scalar("SELECT encode(token_hash, 'hex') FROM password_reset_tokens"),
            expected
        );
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn issue_password_reset_refuses_disabled_accounts_and_revokes_sessions() {
        let fx = Fixture::new();
        let db_secret = fx.secret();
        let env = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", db_secret.as_str()),
            ("CRAFTCLOUD_PUBLIC_URL", "https://cloud.example.org"),
        ];
        assert_eq!(run(&["migrate"], &env).status.code(), Some(0));
        fx.exec("INSERT INTO users (id, normalized_email, display_name) VALUES ('11111111-1111-1111-1111-111111111111', 'sal@example.test', 'Sal')");
        fx.exec("INSERT INTO users (id, normalized_email, display_name, disabled_at) VALUES ('22222222-2222-2222-2222-222222222222', 'dis@example.test', 'Dis', now())");
        fx.exec("INSERT INTO sessions (id, user_id, token_hash, created_at, last_seen_at, absolute_expires_at) VALUES (gen_random_uuid(), '11111111-1111-1111-1111-111111111111', decode(repeat('ab', 32), 'hex'), now(), now(), now() + interval '1 day')");

        let args = ["admin", "issue-password-reset", "--email"];
        let disabled = run(&[args[0], args[1], args[2], "dis@example.test"], &env);
        assert_eq!(disabled.status.code(), Some(1), "{}", text(&disabled));
        assert!(text(&disabled).contains("disabled"));
        assert!(!String::from_utf8_lossy(&disabled.stdout).contains("reset-password#token="));
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM password_reset_tokens"),
            "0"
        );

        let ok = run(&[args[0], args[1], args[2], "sal@example.test"], &env);
        assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
        // The compromise response: active sessions are revoked, the CLI says so.
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM sessions WHERE revoked_at IS NULL"),
            "0"
        );
        assert!(String::from_utf8_lossy(&ok.stderr).contains("1 active session"));
        // A second link voids the first.
        let again = run(&[args[0], args[1], args[2], "sal@example.test"], &env);
        assert_eq!(again.status.code(), Some(0));
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM password_reset_tokens WHERE used_at IS NULL"),
            "1"
        );
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM password_reset_tokens"),
            "2"
        );
    }

    #[test]
    fn set_org_owner_needs_exact_arguments_and_a_database() {
        for args in [
            &["admin", "set-org-owner"][..],
            &["admin", "set-org-owner", "--org", "acme"][..],
            &[
                "admin",
                "set-org-owner",
                "--email",
                "a@b.test",
                "--org",
                "acme",
            ][..],
            &["admin", "set-org-owner", "--org", "acme", "--email"][..],
        ] {
            let out = run(args, &[]);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
            assert!(text(&out).contains("usage: craftcloud-server"));
        }
        let out = run(
            &[
                "admin",
                "set-org-owner",
                "--org",
                "acme",
                "--email",
                "a@b.test",
            ],
            &[],
        );
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn set_org_owner_recovers_an_organization_without_an_enabled_owner() {
        let fx = Fixture::new();
        let db_secret = fx.secret();
        let env = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", db_secret.as_str()),
            ("CRAFTCLOUD_LOG_FORMAT", "json"),
        ];
        assert_eq!(run(&["migrate"], &env).status.code(), Some(0));
        fx.exec("INSERT INTO users (id, normalized_email, display_name, disabled_at) VALUES ('11111111-1111-1111-1111-111111111111', 'gone@example.test', 'Gone', now())");
        fx.exec("INSERT INTO users (id, normalized_email, display_name) VALUES ('22222222-2222-2222-2222-222222222222', 'rescue@example.test', 'Rescue')");
        fx.exec("INSERT INTO organizations (id, slug, name, owner_user_id) VALUES ('33333333-3333-3333-3333-333333333333', 'acme', 'Acme', '11111111-1111-1111-1111-111111111111')");
        fx.exec("INSERT INTO organization_members (org_id, user_id, role, status) VALUES ('33333333-3333-3333-3333-333333333333', '11111111-1111-1111-1111-111111111111', 'owner', 'active')");

        let cmd = |org: &str, email: &str| {
            run(
                &["admin", "set-org-owner", "--org", org, "--email", email],
                &env,
            )
        };
        // Wrong inputs fail without changing anything.
        assert_eq!(cmd("nope", "rescue@example.test").status.code(), Some(1));
        assert_eq!(cmd("acme", "nobody@example.test").status.code(), Some(1));
        let disabled = cmd("acme", "gone@example.test");
        assert_eq!(disabled.status.code(), Some(1));
        assert!(text(&disabled).contains("disabled"));
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM organization_members"),
            "1"
        );

        let ok = cmd("acme", "Rescue@Example.test");
        assert_eq!(ok.status.code(), Some(0), "{}", text(&ok));
        let stdout = String::from_utf8_lossy(&ok.stdout);
        assert!(stdout.contains("added as owner"), "{stdout}");
        assert_eq!(fx.scalar("SELECT role FROM organization_members WHERE user_id = '22222222-2222-2222-2222-222222222222'"), "owner");
        // Audit line with identifiers only: no e-mail address.
        assert!(stdout.contains("org.break_glass_set_owner"), "{stdout}");
        assert!(
            stdout.contains("33333333-3333-3333-3333-333333333333")
                && stdout.contains("22222222-2222-2222-2222-222222222222")
        );
        assert!(!text(&ok).contains("rescue@example.test") && !text(&ok).contains("Rescue"));
        // Repeating is harmless.
        let again = cmd("acme", "rescue@example.test");
        assert_eq!(again.status.code(), Some(0));
        assert!(String::from_utf8_lossy(&again.stdout).contains("already an owner"));
    }

    #[test]
    fn storage_bind_needs_force_a_database_and_a_storage_directory() {
        for args in [
            &["admin", "storage-bind"][..],
            &["admin", "storage-bind", "--yes"][..],
            &["admin", "storage-bind", "--force", "extra"][..],
        ] {
            let out = run(args, &[]);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
            assert!(text(&out).contains("usage: craftcloud-server"));
        }
        let out = run(&["admin", "storage-bind", "--force"], &[]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
    }

    #[test]
    fn maintenance_needs_exact_arguments_and_a_database() {
        for args in [
            &["admin", "maintenance"][..],
            &["admin", "maintenance", "--dry-run"][..],
            &["admin", "maintenance", "--dry-run", "--once"][..],
            &["admin", "maintenance", "--once", "--force"][..],
        ] {
            let out = run(args, &[]);
            assert_eq!(out.status.code(), Some(2), "{args:?}");
            assert!(text(&out).contains("usage: craftcloud-server"));
        }
        let out = run(&["admin", "maintenance", "--once"], &[]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
        // Switched off: the command line refuses as well.
        let off = run(
            &["admin", "maintenance", "--once"],
            &[("CRAFTCLOUD_MAINTENANCE_ENABLED", "false")],
        );
        assert_eq!(off.status.code(), Some(1), "{}", text(&off));
        assert!(text(&off).contains("switched off"));
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn maintenance_once_dry_run_changes_nothing_and_a_real_run_purges() {
        let fx = Fixture::new();
        let db_secret = fx.secret();
        let storage = std::env::temp_dir().join(format!("craftcloud-cli-storage-{}", nanos()));
        std::fs::create_dir_all(&storage).unwrap();
        let storage_text = storage.to_string_lossy().into_owned();
        let env = [
            ("CRAFTCLOUD_DATABASE_URL_FILE", db_secret.as_str()),
            ("CRAFTCLOUD_STORAGE_DIR", storage_text.as_str()),
            ("CRAFTCLOUD_LOG_FORMAT", "json"),
        ];
        // Not migrated yet: refused with a hint.
        let early = run(&["admin", "maintenance", "--once"], &env);
        assert_eq!(early.status.code(), Some(1), "{}", text(&early));
        assert!(text(&early).contains("migrate"));
        assert_eq!(run(&["migrate"], &env).status.code(), Some(0));
        fx.exec("INSERT INTO users (id, normalized_email, display_name) VALUES ('11111111-1111-1111-1111-111111111111', 'o@example.test', 'O')");
        fx.exec("INSERT INTO organizations (id, slug, name, owner_user_id) VALUES ('33333333-3333-3333-3333-333333333333', 'acme', 'Acme', '11111111-1111-1111-1111-111111111111')");
        fx.exec("INSERT INTO workspaces (id, org_id, slug, name) VALUES ('44444444-4444-4444-4444-444444444444', '33333333-3333-3333-3333-333333333333', 'main', 'Main')");
        fx.exec("INSERT INTO projects (id, org_id, workspace_id, name, name_key, created_by_user_id, created_at, updated_by_user_id, updated_at, deleted_at, deleted_by_user_id) VALUES ('55555555-5555-5555-5555-555555555555', '33333333-3333-3333-3333-333333333333', '44444444-4444-4444-4444-444444444444', 'Old', 'old', '11111111-1111-1111-1111-111111111111', now() - interval '90 days', '11111111-1111-1111-1111-111111111111', now() - interval '90 days', now() - interval '60 days', '11111111-1111-1111-1111-111111111111')");

        let dry = run(&["admin", "maintenance", "--once", "--dry-run"], &env);
        assert_eq!(dry.status.code(), Some(0), "{}", text(&dry));
        let stdout = String::from_utf8_lossy(&dry.stdout).into_owned();
        assert!(stdout.contains("nothing was changed"), "{stdout}");
        assert!(stdout.contains("projects_due=1"), "{stdout}");
        fx.assert_no_password(&dry);
        assert_eq!(fx.scalar("SELECT count(*)::text FROM projects"), "1");
        assert_eq!(
            fx.scalar("SELECT count(*)::text FROM maintenance_runs"),
            "0"
        );

        let real = run(&["admin", "maintenance", "--once"], &env);
        assert_eq!(real.status.code(), Some(0), "{}", text(&real));
        let stdout = String::from_utf8_lossy(&real.stdout).into_owned();
        assert!(stdout.contains("purged_projects=1"), "{stdout}");
        // Counts only: no names, no ids.
        assert!(
            !stdout.contains("Old") && !stdout.contains("5555"),
            "{stdout}"
        );
        assert_eq!(fx.scalar("SELECT count(*)::text FROM projects"), "0");
        assert_eq!(
            fx.scalar("SELECT triggered_by FROM maintenance_runs"),
            "cli"
        );

        // The first real run bound database and store to each other.
        assert_eq!(fx.scalar("SELECT count(*)::text FROM storage_binding"), "1");
        assert!(storage.join("craftcloud-store.json").exists());
        // A database that lost the binding does not own the store: exit 1, no change.
        fx.exec("DELETE FROM storage_binding");
        let refused = run(&["admin", "maintenance", "--once"], &env);
        assert_eq!(refused.status.code(), Some(1), "{}", text(&refused));
        let stdout = String::from_utf8_lossy(&refused.stdout).into_owned();
        assert!(stdout.contains("STORE_BINDING_MISMATCH"), "{stdout}");
        assert_eq!(fx.scalar("SELECT count(*)::text FROM storage_binding"), "0");
        // Rebinding is deliberate and needs --force.
        let no_force = run(&["admin", "storage-bind"], &env);
        assert_eq!(no_force.status.code(), Some(2));
        assert_eq!(fx.scalar("SELECT count(*)::text FROM storage_binding"), "0");
        let bound = run(&["admin", "storage-bind", "--force"], &env);
        assert_eq!(bound.status.code(), Some(0), "{}", text(&bound));
        let stdout = String::from_utf8_lossy(&bound.stdout).into_owned();
        assert!(stdout.contains("database binding was: none"), "{stdout}");
        assert_eq!(fx.scalar("SELECT count(*)::text FROM storage_binding"), "1");
        let again = run(&["admin", "maintenance", "--once"], &env);
        assert_eq!(again.status.code(), Some(0), "{}", text(&again));
        let _ = std::fs::remove_dir_all(&storage);
    }
}
