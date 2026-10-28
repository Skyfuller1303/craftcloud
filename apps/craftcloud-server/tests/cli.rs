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

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
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
        assert!(text(&out).contains("applied 2 migration(s)"));
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
}
