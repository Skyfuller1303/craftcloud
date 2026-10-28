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
    fn missing_secret_file_exits_2() {
        let path = std::env::temp_dir().join("craftcloud-cli-test-does-not-exist");
        let path = path.to_string_lossy().into_owned();
        let out = run(&[], &[("CRAFTCLOUD_DATABASE_URL_FILE", &path)]);
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).contains("CRAFTCLOUD_DATABASE_URL_FILE"));
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
            let admin_url = std::env::var("CRAFTCLOUD_TEST_DATABASE_URL")
                .expect("CRAFTCLOUD_TEST_DATABASE_URL must be set");
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
        assert!(text(&out).contains("applied 3 migration(s)"));
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
}
