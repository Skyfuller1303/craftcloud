//! Process-level crash tests against the real `craftcloud-server` binary
//! (CC-P02-08).
//!
//! The binary is built with the `fault-injection` feature (this test target
//! has `required-features = ["fault-injection"]`, so `cargo xtask db-test` and
//! `cargo test -p craftcloud-server --features fault-injection` run it) and
//! aborts itself at a named crash point (`CC_FAULT_ABORT_AT`). Each scenario
//! starts the server, drives HTTP until the process dies, starts a normal
//! server on the same database and store and checks what survived: no visible
//! version with missing or partial content, the previous version still
//! downloads with the right SHA-256, reservations and counters return to the
//! right values, and no object is left unaccounted for.
//!
//! All tests need PostgreSQL (`cargo xtask db-test`) and are `#[ignore]`d
//! otherwise, like the other database tests. The database URL comes from
//! `CC_TEST_DATABASE_URL`; the servers only get it through a secret file.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    use base64::Engine;
    use craftcloud_storage::{LocalStore, ObjectKey, ObjectStore, ReadSource, WrappedDataKey};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use sqlx::postgres::PgConnectOptions;
    use sqlx::{Connection, PgConnection, PgPool};

    const BIN: &str = env!("CARGO_BIN_EXE_craftcloud-server");
    const PASSWORD: &str = "correct-horse-battery-staple";
    const OP_EMAIL: &str = "op@example.test";
    const START_TIMEOUT: Duration = Duration::from_secs(30);
    const DEATH_TIMEOUT: Duration = Duration::from_secs(30);
    const IO_TIMEOUT: Duration = Duration::from_secs(60);
    /// Crash points compare against bytes flushed to the `.part` file.
    const MID_STREAM_BYTES: u64 = 256 * 1024;

    fn write_archive_test_key(path: &Path) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path).unwrap();
        file.write_all(
            base64::engine::general_purpose::STANDARD
                .encode([0x42; 32])
                .as_bytes(),
        )
        .unwrap();
    }

    fn unique() -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!(
            "{}_{nanos}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn sha_hex(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_add(seed).wrapping_mul(31))
            .collect()
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    // ---- files -----------------------------------------------------------------

    fn files_below(dir: &Path) -> BTreeSet<PathBuf> {
        let mut found = BTreeSet::new();
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    found.extend(files_below(&path));
                } else {
                    found.insert(path);
                }
            }
        }
        found
    }

    /// Makes a file look three days old (older than the upload TTL plus the
    /// reconciliation grace period).
    fn age_file(path: &Path) {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(path, permissions).unwrap();
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(3 * 24 * 3600))
            .unwrap();
    }

    // ---- database and directories ---------------------------------------------------

    /// A throwaway database, a store directory and a log directory, removed on
    /// drop.
    struct Fixture {
        rt: tokio::runtime::Runtime,
        admin: PgConnectOptions,
        db_name: String,
        pool: PgPool,
        secret_path: PathBuf,
        root: PathBuf,
        storage: PathBuf,
        logs: PathBuf,
        op_id: String,
        runs: AtomicU32,
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
            let db_name = format!("cc_test_crash_{}", unique());
            rt.block_on(async {
                let mut conn = PgConnection::connect_with(&admin).await.unwrap();
                sqlx::query(&format!("CREATE DATABASE \"{db_name}\""))
                    .execute(&mut conn)
                    .await
                    .unwrap();
            });
            let (base, _) = admin_url.rsplit_once('/').unwrap();
            let url = format!("{base}/{db_name}");
            let root = std::env::temp_dir().join(format!("cc-crash-{}", unique()));
            let storage = root.join("store");
            let logs = root.join("logs");
            std::fs::create_dir_all(&storage).unwrap();
            std::fs::create_dir_all(&logs).unwrap();
            let secret_path = root.join("database-url");
            let mut file = File::create(&secret_path).unwrap();
            writeln!(file, "{url}").unwrap();
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&secret_path, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
            }
            let pool = rt.block_on(async {
                let pool = craftcloud_db::connect(&url, 5, Duration::from_secs(20))
                    .await
                    .unwrap();
                craftcloud_db::migrate::run(&pool).await.unwrap();
                pool
            });
            let hash = craftcloud_auth::hash_password(PASSWORD).unwrap();
            let op_id = rt
                .block_on(craftcloud_db::setup::complete(
                    &pool,
                    &craftcloud_db::setup::NewInstance {
                        operator_email: OP_EMAIL.to_owned(),
                        operator_display_name: "Operator".to_owned(),
                        operator_password_hash: hash,
                        organization_name: "Operator Org".to_owned(),
                        organization_slug: "operator-org".to_owned(),
                    },
                ))
                .unwrap()
                .user_id
                .to_string();
            Self {
                rt,
                admin,
                db_name,
                pool,
                secret_path,
                root,
                storage,
                logs,
                op_id,
                runs: AtomicU32::new(0),
            }
        }

        fn scalar(&self, sql: &str) -> i64 {
            self.rt
                .block_on(sqlx::query_scalar::<_, i64>(sql).fetch_one(&self.pool))
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
        }

        fn text(&self, sql: &str) -> String {
            self.rt
                .block_on(sqlx::query_scalar::<_, String>(sql).fetch_one(&self.pool))
                .unwrap_or_else(|e| panic!("{sql}: {e}"))
        }

        fn exec(&self, sql: &str) {
            self.rt
                .block_on(sqlx::query(sql).execute(&self.pool))
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }

        fn objects(&self) -> BTreeSet<PathBuf> {
            files_below(&self.storage.join("objects"))
        }

        fn temps(&self) -> BTreeSet<PathBuf> {
            files_below(&self.storage.join("tmp"))
        }

        fn quarantined(&self) -> BTreeSet<PathBuf> {
            files_below(&self.storage.join(craftcloud_storage::QUARANTINE_DIR))
        }

        /// A command for the server binary with a clean `CRAFTCLOUD_*` /
        /// `CC_FAULT_*` environment, the database only through the secret file
        /// and both output streams appended to a log file.
        fn command(&self, args: &[&str], env: &[(&str, &str)]) -> (Command, PathBuf) {
            let mut cmd = Command::new(BIN);
            cmd.args(args);
            for (key, _) in std::env::vars() {
                let upper = key.to_ascii_uppercase();
                if upper.starts_with("CRAFTCLOUD_") || upper.starts_with("CC_FAULT_") {
                    cmd.env_remove(key);
                }
            }
            cmd.env(
                "CRAFTCLOUD_DATABASE_URL_FILE",
                self.secret_path.to_string_lossy().as_ref(),
            );
            cmd.env(
                "CRAFTCLOUD_STORAGE_DIR",
                self.storage.to_string_lossy().as_ref(),
            );
            for (key, value) in env {
                cmd.env(key, value);
            }
            let log = self.logs.join(format!(
                "run-{}.log",
                self.runs.fetch_add(1, Ordering::Relaxed)
            ));
            let out = File::create(&log).unwrap();
            cmd.stdout(Stdio::from(out.try_clone().unwrap()))
                .stderr(Stdio::from(out))
                .stdin(Stdio::null());
            (cmd, log)
        }

        /// Starts the HTTP server (maintenance loop off: tests run
        /// maintenance explicitly) and waits until it answers.
        fn serve(&self, abort_at: Option<&str>) -> Server {
            self.serve_configured(abort_at, None)
        }

        fn serve_with_secret_key(
            &self,
            abort_at: Option<&str>,
            secret_key_file: Option<&Path>,
        ) -> Server {
            self.serve_configured(abort_at, secret_key_file)
        }

        fn serve_configured(
            &self,
            abort_at: Option<&str>,
            secret_key_file: Option<&Path>,
        ) -> Server {
            let port = free_port();
            let bind = format!("127.0.0.1:{port}");
            let public = format!("http://127.0.0.1:{port}");
            let mut env = vec![
                ("CRAFTCLOUD_BIND", bind.as_str()),
                ("CRAFTCLOUD_PUBLIC_URL", public.as_str()),
                ("CRAFTCLOUD_MAINTENANCE_ENABLED", "false"),
                ("CRAFTCLOUD_SHUTDOWN_GRACE_SECS", "1"),
            ];
            if let Some(point) = abort_at {
                env.push(("CC_FAULT_ABORT_AT", point));
            }
            let secret_key = secret_key_file.map(|path| {
                (
                    "CRAFTCLOUD_SECRET_KEY_FILE",
                    path.to_str().expect("test secret path is UTF-8"),
                )
            });
            if let Some(secret_key) = secret_key.as_ref() {
                env.push(*secret_key);
            }
            let (mut cmd, log) = self.command(&[], &env);
            let child = cmd.spawn().unwrap();
            let mut server = Server {
                child: Some(child),
                port,
                log,
            };
            let deadline = Instant::now() + START_TIMEOUT;
            loop {
                if let Some(status) = server.child.as_mut().unwrap().try_wait().unwrap() {
                    panic!("server exited early ({status}):\n{}", server.log_text());
                }
                if let Ok(resp) = request(port, "GET", "/health/live", &[], &[])
                    && resp.status == 200
                {
                    return server;
                }
                assert!(
                    Instant::now() < deadline,
                    "server did not start:\n{}",
                    server.log_text()
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }

        /// Runs the CLI to completion (bounded) and returns status and output.
        fn cli(&self, args: &[&str], abort_at: Option<&str>) -> (ExitStatus, String) {
            let mut env = Vec::new();
            if let Some(point) = abort_at {
                env.push(("CC_FAULT_ABORT_AT", point));
            }
            let (mut cmd, log) = self.command(args, &env);
            let mut child = cmd.spawn().unwrap();
            let status = wait_for_exit(&mut child, DEATH_TIMEOUT)
                .unwrap_or_else(|| panic!("CLI {args:?} did not finish"));
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            (status, text)
        }

        fn archive_recall_process(
            &self,
            archive_dir: &Path,
            archive_key_file: &Path,
            abort_at: Option<&str>,
        ) -> (ExitStatus, String) {
            let archive_dir = archive_dir.to_string_lossy().into_owned();
            let archive_key_file = archive_key_file.to_string_lossy().into_owned();
            let mut env = vec![
                ("CC_FAULT_ARCHIVE_LOCAL_DIR", archive_dir.as_str()),
                ("CRAFTCLOUD_SECRET_KEY_FILE", archive_key_file.as_str()),
            ];
            if let Some(point) = abort_at {
                env.push(("CC_FAULT_ABORT_AT", point));
            }
            let (mut cmd, log) = self.command(&["archive", "recall"], &env);
            let mut child = cmd.spawn().unwrap();
            let status = wait_for_exit(&mut child, DEATH_TIMEOUT)
                .unwrap_or_else(|| panic!("archive recall process timed out"));
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            (status, text)
        }

        /// One maintenance run that must succeed; returns its report.
        fn maintenance(&self) -> String {
            let (status, text) = self.cli(&["admin", "maintenance", "--once"], None);
            assert!(status.success(), "maintenance failed ({status}):\n{text}");
            text
        }

        /// Closes all open upload sessions as expired by moving their expiry
        /// into the past (the next maintenance run releases their quota).
        fn expire_open_sessions(&self) {
            self.exec(
                "UPDATE upload_sessions SET expires_at = now() - interval '2 hours' \
                 WHERE state IN ('created', 'receiving', 'stored')",
            );
        }

        /// Reserved and committed bytes must equal what the rows say.
        fn assert_counters(&self, reserved: i64, committed: i64) {
            for (table, column, want) in [
                ("organizations", "reserved_bytes", reserved),
                ("workspaces", "reserved_bytes", reserved),
                ("organizations", "committed_bytes", committed),
                ("workspaces", "committed_bytes", committed),
            ] {
                let got = self.scalar(&format!(
                    "SELECT COALESCE(sum({column}), 0)::bigint FROM {table}"
                ));
                assert_eq!(got, want, "{table}.{column}");
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.rt.block_on(self.pool.close());
            let admin = self.admin.clone();
            let name = self.db_name.clone();
            self.rt.block_on(async {
                if let Ok(mut conn) = PgConnection::connect_with(&admin).await {
                    let _ =
                        sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                            .execute(&mut conn)
                            .await;
                }
            });
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    // ---- processes -----------------------------------------------------------------

    /// A running server; killed and reaped on drop, also when a test fails.
    struct Server {
        child: Option<Child>,
        port: u16,
        log: PathBuf,
    }

    impl Server {
        fn log_text(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }

        /// Waits for the process to die by itself and returns how.
        fn died(&mut self) -> ExitStatus {
            let child = self.child.as_mut().unwrap();
            let status = wait_for_exit(child, DEATH_TIMEOUT).unwrap_or_else(|| {
                panic!(
                    "the server did not abort at the crash point:\n{}",
                    self.log_text()
                )
            });
            self.child = None;
            status
        }

        /// Stops a healthy server (TerminateProcess / SIGKILL; nothing in these
        /// tests depends on a graceful stop).
        fn kill(mut self) {
            self.reap();
        }

        fn reap(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.reap();
        }
    }

    fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// An abnormal end: not a success and not one of the server's own exit
    /// codes (1 runtime failure, 2 invalid configuration). `abort()` is
    /// SIGABRT on unix and a fast-fail (0xC0000409) on Windows.
    fn assert_aborted(status: ExitStatus, log: &str) {
        let code = status.code();
        assert!(
            !status.success() && !matches!(code, Some(0..=2)),
            "expected an abort, got {status}:\n{log}"
        );
        assert!(
            log.contains("fault injection: aborting the process at crash point"),
            "the process ended, but not at the crash point:\n{log}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(status.signal(), Some(6), "{status}");
        }
        #[cfg(windows)]
        assert_eq!(code.map(|c| c as u32), Some(0xC000_0409), "{status}");
    }

    fn archive_worker_child(
        fixture: &Fixture,
        archive_dir: &Path,
        archive_key_file: &Path,
        abort_at: Option<&str>,
    ) -> std::process::Output {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "tests::archive_worker_crash_child",
            "--nocapture",
        ]);
        command.env("CC_FAULT_ARCHIVE_CHILD", "1");
        command.env("CC_FAULT_ARCHIVE_DB_FILE", &fixture.secret_path);
        command.env("CC_FAULT_ARCHIVE_STORAGE_DIR", &fixture.storage);
        command.env("CC_FAULT_ARCHIVE_LOCAL_DIR", archive_dir);
        command.env("CC_FAULT_ARCHIVE_KEY_FILE", archive_key_file);
        if let Some(point) = abort_at {
            command.env("CC_FAULT_ABORT_AT", point);
        } else {
            command.env_remove("CC_FAULT_ABORT_AT");
        }
        command.output().unwrap()
    }

    fn assert_archive_bytes(
        fixture: &Fixture,
        version_id: &str,
        archive_dir: &Path,
        expected: &[u8],
    ) {
        let row: (uuid::Uuid, String, Vec<u8>, i64, Vec<u8>) = fixture
            .rt
            .block_on(async {
                sqlx::query_as(
                    "SELECT b.id, b.archive_object_key, b.archive_wrapped_key, b.size_bytes, b.sha256 \
                     FROM blobs b JOIN file_versions v ON v.blob_id=b.id WHERE v.id=$1",
                )
                .bind(uuid::Uuid::parse_str(version_id).unwrap())
                .fetch_one(&fixture.pool)
                .await
            })
            .unwrap();
        let (blob_id, key_text, wrapped, size, digest) = row;
        assert_eq!(size, expected.len() as i64);
        assert_eq!(digest.as_slice(), Sha256::digest(expected).as_slice());
        let archive = fixture.rt.block_on(LocalStore::open(archive_dir)).unwrap();
        let key = ObjectKey::parse(&key_text).unwrap();
        let envelope = WrappedDataKey::from_bytes(&wrapped).unwrap();
        let crypto = craftcloud_storage::ArchiveCrypto::from_server_key(&[0x42; 32]).unwrap();
        let mut reader = fixture
            .rt
            .block_on(craftcloud_api::archive_transport::open_archive_range(
                &archive,
                &crypto,
                &key,
                &envelope,
                blob_id,
                u64::try_from(size).unwrap(),
                None,
            ))
            .unwrap();
        let actual = fixture.rt.block_on(async {
            let mut bytes = Vec::new();
            while let Some(chunk) = reader.next_chunk().await.unwrap() {
                bytes.extend_from_slice(&chunk);
            }
            bytes
        });
        assert_eq!(actual, expected);
    }

    fn assert_local_version_bytes(fixture: &Fixture, version_id: &str, expected: &[u8]) {
        let (key_text, digest): (String, Vec<u8>) = fixture
            .rt
            .block_on(async {
                sqlx::query_as(
                    "SELECT b.object_key, b.sha256 FROM blobs b \
                     JOIN file_versions v ON v.blob_id=b.id WHERE v.id=$1",
                )
                .bind(uuid::Uuid::parse_str(version_id).unwrap())
                .fetch_one(&fixture.pool)
                .await
            })
            .unwrap();
        assert_eq!(digest.as_slice(), Sha256::digest(expected).as_slice());
        let local = fixture
            .rt
            .block_on(LocalStore::open(&fixture.storage))
            .unwrap();
        let key = ObjectKey::parse(&key_text).unwrap();
        let mut reader = fixture.rt.block_on(local.open_read(&key, None)).unwrap();
        let actual = fixture.rt.block_on(async {
            let mut bytes = Vec::new();
            while let Some(chunk) = reader.next_chunk().await.unwrap() {
                bytes.extend_from_slice(&chunk);
            }
            bytes
        });
        assert_eq!(actual, expected);
    }

    type ChunkCrashEntry = (
        i32,
        i64,
        uuid::Uuid,
        String,
        i64,
        Vec<u8>,
        String,
        i64,
        String,
        Option<String>,
        Option<Vec<u8>>,
    );

    fn assert_chunked_version_bytes(
        fixture: &Fixture,
        version_id: &str,
        archive_dir: &Path,
        expected: &[u8],
    ) {
        let chunks: Vec<ChunkCrashEntry> = fixture
            .rt
            .block_on(async {
                sqlx::query_as(
                    "SELECT vc.ordinal,vc.byte_offset,c.id,c.object_key,c.size_bytes,c.sha256,c.encoding,c.stored_bytes, \
                            c.storage_tier,c.archive_object_key,c.archive_wrapped_key \
                     FROM version_chunks vc JOIN chunks c ON c.id=vc.chunk_id \
                     WHERE vc.version_id=$1 ORDER BY vc.ordinal",
                )
                .bind(uuid::Uuid::parse_str(version_id).unwrap())
                .fetch_all(&fixture.pool)
                .await
            })
            .unwrap();
        assert!(!chunks.is_empty());
        let local = fixture
            .rt
            .block_on(LocalStore::open(&fixture.storage))
            .unwrap();
        let archive = fixture.rt.block_on(LocalStore::open(archive_dir)).unwrap();
        let crypto = craftcloud_storage::ArchiveCrypto::from_server_key(&[0x42; 32]).unwrap();
        let actual = fixture.rt.block_on(async {
            let mut bytes = Vec::new();
            let mut offset = 0_i64;
            for (
                index,
                (
                    ordinal,
                    chunk_offset,
                    chunk_id,
                    local_text,
                    size,
                    raw_sha,
                    encoding,
                    stored_size,
                    tier,
                    archive_text,
                    wrapped,
                ),
            ) in chunks.into_iter().enumerate()
            {
                assert_eq!(usize::try_from(ordinal).unwrap(), index);
                assert_eq!(chunk_offset, offset);
                let size = u64::try_from(size).unwrap();
                let stored_size = u64::try_from(stored_size).unwrap();
                let expected_sha: [u8; 32] = raw_sha.as_slice().try_into().unwrap();
                let mut chunk_bytes = Vec::new();
                if tier == "local" {
                    let key = ObjectKey::parse(&local_text).unwrap();
                    let mut reader = local.open_read(&key, None).await.unwrap();
                    assert_eq!(reader.remaining(), stored_size);
                    while let Some(block) = reader.next_chunk().await.unwrap() {
                        chunk_bytes.extend_from_slice(&block);
                    }
                } else {
                    assert_eq!(tier, "archive");
                    let key = ObjectKey::parse(archive_text.as_deref().unwrap()).unwrap();
                    let envelope = WrappedDataKey::from_bytes(wrapped.as_deref().unwrap()).unwrap();
                    let mut reader = craftcloud_api::archive_transport::open_archive_range(
                        &archive,
                        &crypto,
                        &key,
                        &envelope,
                        chunk_id,
                        stored_size,
                        None,
                    )
                    .await
                    .unwrap();
                    while let Some(block) = reader.next_chunk().await.unwrap() {
                        chunk_bytes.extend_from_slice(&block);
                    }
                }
                assert_eq!(chunk_bytes.len() as u64, stored_size);
                let chunk_bytes = craftcloud_storage::compression::decode_chunk(
                    &encoding,
                    &chunk_bytes,
                    size,
                    &expected_sha,
                )
                .unwrap();
                assert_eq!(chunk_bytes.len() as u64, size);
                assert_eq!(Sha256::digest(&chunk_bytes).as_slice(), expected_sha);
                offset += i64::try_from(size).unwrap();
                bytes.extend_from_slice(&chunk_bytes);
            }
            bytes
        });
        assert_eq!(actual, expected);
        assert_eq!(
            Sha256::digest(&actual).as_slice(),
            Sha256::digest(expected).as_slice()
        );
    }

    // ---- minimal HTTP client -------------------------------------------------------

    struct Resp {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Resp {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }

        fn json(&self) -> Value {
            serde_json::from_slice(&self.body).unwrap_or(Value::Null)
        }

        fn text(&self) -> String {
            String::from_utf8_lossy(&self.body).into_owned()
        }
    }

    fn connect(port: u16) -> std::io::Result<TcpStream> {
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(stream)
    }

    fn head_bytes(
        method: &str,
        path: &str,
        port: u16,
        headers: &[(String, String)],
        content_length: usize,
    ) -> Vec<u8> {
        let mut head =
            format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if content_length > 0 || method != "GET" {
            head.push_str(&format!("Content-Length: {content_length}\r\n"));
        }
        head.push_str("\r\n");
        head.into_bytes()
    }

    fn decode_chunked(mut raw: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(pos) = raw.windows(2).position(|w| w == b"\r\n") {
            let size_text = String::from_utf8_lossy(&raw[..pos]).into_owned();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("0").trim(), 16)
                .unwrap_or(0);
            raw = &raw[pos + 2..];
            if size == 0 || raw.len() < size {
                break;
            }
            out.extend_from_slice(&raw[..size]);
            raw = raw.get(size + 2..).unwrap_or(&[]);
        }
        out
    }

    /// One request, `Connection: close`. Any transport failure, including a
    /// connection that ends before a full response head arrived (the server
    /// died), is an `Err`.
    fn request(
        port: u16,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> std::io::Result<Resp> {
        let mut stream = connect(port)?;
        stream.write_all(&head_bytes(method, path, port, headers, body.len()))?;
        stream.write_all(body)?;
        read_response(&mut stream)
    }

    fn read_response(stream: &mut TcpStream) -> std::io::Result<Resp> {
        let mut raw = Vec::new();
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&chunk[..n]),
                // A reset after some data still lets us judge the head below.
                Err(e) if !raw.is_empty() && e.kind() == std::io::ErrorKind::ConnectionReset => {
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        let eof = || std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no complete response");
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or_else(eof)?;
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|l| l.split(' ').nth(1))
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(eof)?;
        let headers: Vec<(String, String)> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
            .collect();
        let rest = &raw[split + 4..];
        let chunked = headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
        let declared = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse::<usize>().ok());
        let body = if chunked {
            decode_chunked(rest)
        } else if let Some(len) = declared {
            if rest.len() < len {
                return Err(eof());
            }
            rest[..len].to_vec()
        } else {
            rest.to_vec()
        };
        Ok(Resp {
            status,
            headers,
            body,
        })
    }

    /// A logged-in operator talking to one server.
    struct Api {
        port: u16,
        cookie: String,
        csrf: String,
    }

    impl Api {
        fn login(port: u16) -> Self {
            let origin = format!("http://127.0.0.1:{port}");
            let resp = request(
                port,
                "POST",
                "/api/v1/auth/login",
                &[
                    ("Origin".to_owned(), origin),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                ],
                json!({"email": OP_EMAIL, "password": PASSWORD})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
            assert_eq!(resp.status, 200, "login: {}", resp.text());
            let set_cookie = resp.header("set-cookie").unwrap();
            let cookie = set_cookie.split(';').next().unwrap().to_owned();
            let csrf = resp.json()["csrf_token"].as_str().unwrap().to_owned();
            Self { port, cookie, csrf }
        }

        fn headers(&self, extra: &[(&str, &str)]) -> Vec<(String, String)> {
            let mut headers = vec![
                ("Cookie".to_owned(), self.cookie.clone()),
                ("X-CSRF-Token".to_owned(), self.csrf.clone()),
                (
                    "Origin".to_owned(),
                    format!("http://127.0.0.1:{}", self.port),
                ),
            ];
            headers.extend(
                extra
                    .iter()
                    .map(|(n, v)| ((*n).to_owned(), (*v).to_owned())),
            );
            headers
        }

        fn send(
            &self,
            method: &str,
            path: &str,
            extra: &[(&str, &str)],
            content_type: Option<&str>,
            body: &[u8],
        ) -> std::io::Result<Resp> {
            let mut headers = self.headers(extra);
            if let Some(kind) = content_type {
                headers.push(("Content-Type".to_owned(), kind.to_owned()));
            }
            request(self.port, method, path, &headers, body)
        }

        fn json_call(
            &self,
            method: &str,
            path: &str,
            extra: &[(&str, &str)],
            body: Option<Value>,
        ) -> std::io::Result<Resp> {
            match body {
                Some(body) => self.send(
                    method,
                    path,
                    extra,
                    Some("application/json"),
                    body.to_string().as_bytes(),
                ),
                None => self.send(method, path, extra, None, &[]),
            }
        }

        fn ok_json(&self, method: &str, path: &str, body: Value, status: u16) -> Value {
            let resp = self.json_call(method, path, &[], Some(body)).unwrap();
            assert_eq!(resp.status, status, "{method} {path}: {}", resp.text());
            resp.json()
        }

        fn create_upload(&self, file: &str, data: &[u8]) -> String {
            let body = json!({"size_bytes": data.len(), "sha256": sha_hex(data)});
            let json = self.ok_json("POST", &format!("/api/v1/files/{file}/uploads"), body, 201);
            json["upload_id"].as_str().unwrap().to_owned()
        }

        fn put_content(&self, upload: &str, data: &[u8]) -> std::io::Result<Resp> {
            self.send(
                "PUT",
                &format!("/api/v1/uploads/{upload}/content"),
                &[],
                Some("application/octet-stream"),
                data,
            )
        }

        fn commit(
            &self,
            file: &str,
            upload: &str,
            precondition: (&str, &str),
        ) -> std::io::Result<Resp> {
            self.json_call(
                "POST",
                &format!("/api/v1/files/{file}/versions"),
                &[precondition],
                Some(json!({"upload_id": upload})),
            )
        }

        /// A complete, committed version; returns the version id.
        fn publish(&self, file: &str, data: &[u8], precondition: (&str, &str)) -> String {
            let upload = self.create_upload(file, data);
            let put = self.put_content(&upload, data).unwrap();
            assert_eq!(put.status, 200, "PUT: {}", put.text());
            let commit = self.commit(file, &upload, precondition).unwrap();
            assert_eq!(commit.status, 201, "commit: {}", commit.text());
            commit.json()["id"].as_str().unwrap().to_owned()
        }

        fn download(&self, path: &str) -> Resp {
            let resp = self.send("GET", path, &[], None, &[]).unwrap();
            assert_eq!(resp.status, 200, "GET {path}: {}", resp.text());
            resp
        }
    }

    fn tag(version: &str) -> String {
        format!("\"version:{version}\"")
    }

    /// Organization, workspace, project and an empty file; returns the file id.
    fn make_file(api: &Api, op_id: &str) -> String {
        let org = api.ok_json(
            "POST",
            "/api/v1/organizations",
            json!({"name": "Org crash", "slug": "crash", "owner_user_id": op_id}),
            201,
        );
        let org = org["id"].as_str().unwrap().to_owned();
        let ws = api.ok_json(
            "POST",
            &format!("/api/v1/organizations/{org}/workspaces"),
            json!({"name": "WS crash", "slug": "main"}),
            201,
        );
        let ws = ws["id"].as_str().unwrap().to_owned();
        let project = api.ok_json(
            "POST",
            &format!("/api/v1/workspaces/{ws}/projects"),
            json!({"name": "P crash"}),
            201,
        );
        let project = project["id"].as_str().unwrap().to_owned();
        let file = api.ok_json(
            "POST",
            &format!("/api/v1/projects/{project}/files"),
            json!({"name": "design.fig", "app": "figcraft", "format": "fig"}),
            201,
        );
        file["id"].as_str().unwrap().to_owned()
    }

    /// The common start of every scenario: a normal server, a file with one
    /// committed version.
    struct World {
        fx: Fixture,
        file: String,
        v1: String,
        v1_data: Vec<u8>,
        /// Object files on disk after the baseline (exactly the first version).
        baseline_objects: BTreeSet<PathBuf>,
    }

    impl World {
        /// Builds the baseline with a normal server; `then` can add more work
        /// before that server is stopped.
        fn new(then: impl FnOnce(&Fixture, &Api, &str)) -> Self {
            Self::new_with_first_size(1000, then)
        }

        fn new_with_first_size(first_size: usize, then: impl FnOnce(&Fixture, &Api, &str)) -> Self {
            Self::new_with_options(first_size, false, then)
        }

        fn new_chunked_with_first_size(
            first_size: usize,
            then: impl FnOnce(&Fixture, &Api, &str),
        ) -> Self {
            Self::new_with_options(first_size, true, then)
        }

        fn new_with_options(
            first_size: usize,
            chunked: bool,
            then: impl FnOnce(&Fixture, &Api, &str),
        ) -> Self {
            let fx = Fixture::new();
            let secret_key_file = if chunked {
                let path = fx.root.join("test-server-key");
                std::fs::write(
                    &path,
                    base64::engine::general_purpose::STANDARD.encode([0x42; 32]),
                )
                .unwrap();
                Some(path)
            } else {
                None
            };
            let server = fx.serve_with_secret_key(None, secret_key_file.as_deref());
            let api = Api::login(server.port);
            let file = make_file(&api, &fx.op_id);
            let v1_data = pattern(first_size, 1);
            let v1 = api.publish(&file, &v1_data, ("If-None-Match", "*"));
            let baseline_objects = fx.objects();
            assert!(!baseline_objects.is_empty(), "{baseline_objects:?}");
            then(&fx, &api, &file);
            server.kill();
            Self {
                fx,
                file,
                v1,
                v1_data,
                baseline_objects,
            }
        }

        /// The first version still downloads, bit for bit, by id and, while it
        /// is the head, as the head.
        fn assert_v1_intact(&self, api: &Api) {
            if self.head_number() == 1 {
                let by_head = api.download(&format!("/api/v1/files/{}/content", self.file));
                assert_eq!(sha_hex(&by_head.body), sha_hex(&self.v1_data));
            }
            let by_id = api.download(&format!(
                "/api/v1/files/{}/versions/{}/content",
                self.file, self.v1
            ));
            assert_eq!(sha_hex(&by_id.body), sha_hex(&self.v1_data));
        }

        /// No version row (and so nothing a client can list or download) lacks
        /// complete content: every version downloads with its recorded size
        /// and SHA-256.
        fn assert_every_version_complete(&self, api: &Api) -> usize {
            let count = self.fx.scalar("SELECT count(*) FROM file_versions");
            for i in 0..count {
                let version = self.fx.text(&format!(
                    "SELECT id::text FROM file_versions ORDER BY created_at, number OFFSET {i} LIMIT 1"
                ));
                let sha = self.fx.text(&format!(
                    "SELECT encode(sha256, 'hex') FROM file_versions WHERE id = '{version}'"
                ));
                let size = self.fx.scalar(&format!(
                    "SELECT size_bytes FROM file_versions WHERE id = '{version}'"
                ));
                let resp = api.download(&format!(
                    "/api/v1/files/{}/versions/{version}/content",
                    self.file
                ));
                assert_eq!(resp.body.len() as i64, size, "version {version} size");
                assert_eq!(sha_hex(&resp.body), sha, "version {version} sha256");
            }
            usize::try_from(count).unwrap()
        }

        fn head_number(&self) -> i64 {
            self.fx.scalar(&format!(
                "SELECT v.number FROM files f JOIN file_versions v ON v.id = f.head_version_id \
                 WHERE f.id = '{}'",
                self.file
            ))
        }
    }

    // ---- scenarios ---------------------------------------------------------------

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn killing_the_server_mid_upload_leaves_no_version_and_expiry_releases_the_reservation() {
        let world = World::new(|_, _, _| {});
        let fx = &world.fx;
        let v2_data = pattern(1024 * 1024, 2);

        // Crash after 256 KiB reached the .part file.
        let point = format!("upload-mid-stream:{MID_STREAM_BYTES}");
        let mut doomed = fx.serve(Some(&point));
        let api = Api::login(doomed.port);
        let upload = api.create_upload(&world.file, &v2_data);
        let sent = api.put_content(&upload, &v2_data);
        assert!(sent.is_err(), "the server answered although it must die");
        let status = doomed.died();
        assert_aborted(status, &doomed.log_text());

        // What the crash left behind: a partial .part file, a receiving session
        // that still holds its reservation, and nothing else.
        let parts = fx.temps();
        assert_eq!(parts.len(), 1, "{parts:?}");
        let part_len = std::fs::metadata(parts.iter().next().unwrap())
            .unwrap()
            .len();
        assert!(
            (MID_STREAM_BYTES..v2_data.len() as u64).contains(&part_len),
            "partial length {part_len}"
        );
        assert_eq!(fx.objects(), world.baseline_objects);
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 1);
        assert_eq!(fx.scalar("SELECT count(*) FROM blobs"), 1);
        assert_eq!(
            fx.text(&format!(
                "SELECT state FROM upload_sessions WHERE id = '{upload}'"
            )),
            "receiving"
        );
        fx.assert_counters(v2_data.len() as i64, world.v1_data.len() as i64);

        // Restart: only version 1 is visible and it is intact.
        let server = fx.serve(None);
        let api = Api::login(server.port);
        assert_eq!(world.head_number(), 1);
        world.assert_v1_intact(&api);
        assert_eq!(world.assert_every_version_complete(&api), 1);
        // The session cannot be committed: it never stored content.
        let commit = api
            .commit(&world.file, &upload, ("If-Match", &tag(&world.v1)))
            .unwrap();
        assert_ne!(commit.status, 201, "{}", commit.text());
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 1);

        // Maintenance: the session expires (reservation released), the old
        // .part file is removed. Nothing of version 1 is touched.
        fx.expire_open_sessions();
        for part in &parts {
            age_file(part);
        }
        let report = fx.maintenance();
        assert!(report.contains("removed_temp=1"), "{report}");
        fx.assert_counters(0, world.v1_data.len() as i64);
        assert!(fx.temps().is_empty(), "{:?}", fx.temps());
        assert_eq!(fx.objects(), world.baseline_objects);
        assert_eq!(fx.scalar("SELECT count(*) FROM blobs"), 1);
        world.assert_v1_intact(&api);
        server.kill();
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn killing_the_server_after_publishing_the_object_but_before_recording_it_leaves_no_version() {
        let world = World::new(|_, _, _| {});
        let fx = &world.fx;
        let v2_data = pattern(300 * 1024, 3);

        let mut doomed = fx.serve(Some("upload-before-record"));
        let api = Api::login(doomed.port);
        let upload = api.create_upload(&world.file, &v2_data);
        assert!(api.put_content(&upload, &v2_data).is_err());
        let status = doomed.died();
        assert_aborted(status, &doomed.log_text());

        // The object was published, but the transaction that records the blob
        // never committed: an unreferenced object, no blob row, no version.
        let objects = fx.objects();
        let stray: Vec<&PathBuf> = objects.difference(&world.baseline_objects).collect();
        assert_eq!(stray.len(), 1, "{objects:?}");
        let stray = stray[0].clone();
        assert_eq!(
            std::fs::metadata(&stray).unwrap().len(),
            v2_data.len() as u64,
            "the published object is complete"
        );
        assert!(fx.temps().is_empty(), "{:?}", fx.temps());
        assert_eq!(fx.scalar("SELECT count(*) FROM blobs"), 1);
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 1);
        assert_eq!(
            fx.text(&format!(
                "SELECT state FROM upload_sessions WHERE id = '{upload}'"
            )),
            "receiving"
        );

        let server = fx.serve(None);
        let api = Api::login(server.port);
        assert_eq!(world.head_number(), 1);
        world.assert_v1_intact(&api);
        assert_eq!(world.assert_every_version_complete(&api), 1);

        // A young stray is left alone (it could belong to a running upload).
        fx.expire_open_sessions();
        let report = fx.maintenance();
        assert!(!report.contains("quarantined_objects"), "{report}");
        assert!(fx.objects().contains(&stray));
        fx.assert_counters(0, world.v1_data.len() as i64);

        // Once it is older than the grace period it is quarantined, not deleted.
        age_file(&stray);
        let report = fx.maintenance();
        assert!(report.contains("quarantined_objects=1"), "{report}");
        assert_eq!(fx.objects(), world.baseline_objects);
        assert_eq!(fx.quarantined().len(), 1, "{:?}", fx.quarantined());
        fx.assert_counters(0, world.v1_data.len() as i64);
        world.assert_v1_intact(&api);
        assert_eq!(world.assert_every_version_complete(&api), 1);
        server.kill();
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn killing_the_server_after_the_commit_but_before_the_response_keeps_a_complete_durable_version()
     {
        let world = World::new(|_, _, _| {});
        let fx = &world.fx;
        let v2_data = pattern(200 * 1024, 4);

        let mut doomed = fx.serve(Some("version-after-commit"));
        let api = Api::login(doomed.port);
        let upload = api.create_upload(&world.file, &v2_data);
        let put = api.put_content(&upload, &v2_data).unwrap();
        assert_eq!(put.status, 200, "{}", put.text());
        let lost = api.commit(&world.file, &upload, ("If-Match", &tag(&world.v1)));
        assert!(lost.is_err(), "the client must not get a response");
        let status = doomed.died();
        assert_aborted(status, &doomed.log_text());

        // The client never saw the answer, but the commit is durable and the
        // version is complete.
        let server = fx.serve(None);
        let api = Api::login(server.port);
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 2);
        assert_eq!(world.head_number(), 2);
        assert_eq!(world.assert_every_version_complete(&api), 2);
        let head = api.download(&format!("/api/v1/files/{}/content", world.file));
        assert_eq!(sha_hex(&head.body), sha_hex(&v2_data));
        world.assert_v1_intact(&api);
        assert_eq!(
            fx.text(&format!(
                "SELECT state FROM upload_sessions WHERE id = '{upload}'"
            )),
            "committed"
        );
        fx.assert_counters(0, (world.v1_data.len() + v2_data.len()) as i64);

        // The client can recover by repeating the commit: it is a replay that
        // returns the same version and creates nothing new.
        let replay = api
            .commit(&world.file, &upload, ("If-Match", &tag(&world.v1)))
            .unwrap();
        assert!(
            replay.status == 200 || replay.status == 201,
            "replay: {} {}",
            replay.status,
            replay.text()
        );
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 2);

        // Maintenance changes nothing: both versions are referenced.
        let before = fx.objects();
        let report = fx.maintenance();
        assert!(!report.contains("quarantined_objects"), "{report}");
        assert!(!report.contains("blobs_deleted"), "{report}");
        assert_eq!(fx.objects(), before);
        assert_eq!(before.len(), 2);
        fx.assert_counters(0, (world.v1_data.len() + v2_data.len()) as i64);
        assert_eq!(world.assert_every_version_complete(&api), 2);
        server.kill();
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn killing_maintenance_between_the_gc_claim_and_the_object_delete_is_finished_by_the_next_run()
    {
        let v2_data = pattern(150 * 1024, 5);
        let mut orphan_upload = String::new();
        let world = World::new(|fx, api, file| {
            // A stored but never committed upload that is then aborted: its
            // blob becomes `orphaned`, a candidate for garbage collection.
            let upload = api.create_upload(file, &v2_data);
            let put = api.put_content(&upload, &v2_data).unwrap();
            assert_eq!(put.status, 200, "{}", put.text());
            let abort = api
                .json_call("DELETE", &format!("/api/v1/uploads/{upload}"), &[], None)
                .unwrap();
            assert!(abort.status < 300, "{}: {}", abort.status, abort.text());
            assert_eq!(
                fx.scalar("SELECT count(*) FROM blobs WHERE state = 'orphaned'"),
                1
            );
            orphan_upload = upload;
        });
        let fx = &world.fx;
        assert!(!orphan_upload.is_empty());
        assert_eq!(fx.objects().len(), 2);
        fx.exec("UPDATE blobs SET created_at = now() - interval '3 days' WHERE state = 'orphaned'");

        // First run dies right after the claim (blob `deleting`) committed.
        let (status, log) = fx.cli(&["admin", "maintenance", "--once"], Some("gc-after-claim"));
        assert_aborted(status, &log);
        assert_eq!(
            fx.scalar("SELECT count(*) FROM blobs WHERE state = 'deleting'"),
            1
        );
        assert_eq!(fx.objects().len(), 2, "the object is still there");
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 1);

        // The crash does not hurt a reader: version 1 is intact.
        let server = fx.serve(None);
        let api = Api::login(server.port);
        assert_eq!(world.head_number(), 1);
        world.assert_v1_intact(&api);
        assert_eq!(world.assert_every_version_complete(&api), 1);
        fx.assert_counters(0, world.v1_data.len() as i64);

        // The next run finishes the job: object and row are gone, exactly once.
        let report = fx.maintenance();
        assert!(report.contains("blobs_deleted=1"), "{report}");
        assert_eq!(fx.scalar("SELECT count(*) FROM blobs"), 1);
        assert_eq!(
            fx.scalar("SELECT count(*) FROM blobs WHERE state = 'deleting'"),
            0
        );
        assert_eq!(fx.objects(), world.baseline_objects);
        assert!(fx.quarantined().is_empty());
        fx.assert_counters(0, world.v1_data.len() as i64);
        world.assert_v1_intact(&api);

        // And a third run has nothing left to do.
        let report = fx.maintenance();
        assert!(!report.contains("blobs_deleted"), "{report}");
        server.kill();
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn a_client_that_drops_the_connection_mid_upload_leaves_no_version_and_releases_the_reservation()
     {
        let world = World::new(|_, _, _| {});
        let fx = &world.fx;
        let v2_data = pattern(1024 * 1024, 6);

        let server = fx.serve(None);
        let api = Api::login(server.port);
        let upload = api.create_upload(&world.file, &v2_data);
        fx.assert_counters(v2_data.len() as i64, world.v1_data.len() as i64);

        // Send the head and half of the body, then close the socket.
        let mut headers = api.headers(&[]);
        headers.push((
            "Content-Type".to_owned(),
            "application/octet-stream".to_owned(),
        ));
        let mut stream = connect(server.port).unwrap();
        stream
            .write_all(&head_bytes(
                "PUT",
                &format!("/api/v1/uploads/{upload}/content"),
                server.port,
                &headers,
                v2_data.len(),
            ))
            .unwrap();
        stream.write_all(&v2_data[..v2_data.len() / 2]).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        stream.shutdown(Shutdown::Both).unwrap();
        drop(stream);

        assert_eq!(world.head_number(), 1);

        // The server notices and cleans up the partial file.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !fx.temps().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(fx.temps().is_empty(), "{:?}", fx.temps());
        assert_eq!(fx.scalar("SELECT count(*) FROM file_versions"), 1);
        assert_eq!(fx.scalar("SELECT count(*) FROM blobs"), 1);
        assert_eq!(fx.objects(), world.baseline_objects);
        world.assert_v1_intact(&api);

        // Whatever state the session ended in, expiry plus maintenance leaves
        // no reservation and nothing else behind.
        fx.expire_open_sessions();
        fx.maintenance();
        fx.assert_counters(0, world.v1_data.len() as i64);
        assert_eq!(
            fx.scalar(&format!(
                "SELECT count(*) FROM upload_sessions WHERE id = '{upload}' \
                 AND state IN ('created', 'receiving', 'stored', 'committed')"
            )),
            0
        );
        assert_eq!(fx.objects(), world.baseline_objects);
        assert!(fx.temps().is_empty());
        assert_eq!(world.assert_every_version_complete(&api), 1);
        server.kill();
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn archive_transport_process_crashes_resume_without_losing_version_bytes() {
        for (point, published, state, local_count, delete_pending) in [
            ("archive-after-intent", false, "archiving", 2, false),
            ("archive-upload-started", false, "archiving", 2, false),
            (
                "archive-upload-mid-stream:65536",
                false,
                "archiving",
                2,
                false,
            ),
            ("archive-upload-published", true, "archiving", 2, false),
            ("archive-upload-verified", true, "archiving", 2, false),
            ("archive-db-committed", true, "archived", 2, true),
            ("archive-local-deleted", true, "archived", 1, true),
        ] {
            let world = World::new_with_first_size(256 * 1024 + 7, |fixture, api, file| {
                let second_data = pattern(1024 * 1024 + 7, 23);
                let version = tag(&fixture.text(&format!(
                    "SELECT head_version_id::text FROM files WHERE id='{file}'"
                )));
                api.publish(file, &second_data, ("If-Match", version.as_str()));
            });
            let fx = &world.fx;
            let archive_dir = fx.root.join(format!("archive-{}", unique()));
            std::fs::create_dir_all(&archive_dir).unwrap();
            let archive_key_file = fx.root.join(format!("archive-key-{}", unique()));
            write_archive_test_key(&archive_key_file);
            let old_blob = fx.text(&format!(
                "SELECT blob_id::text FROM file_versions WHERE id='{}'",
                world.v1
            ));
            let head_version = fx.text(&format!(
                "SELECT head_version_id::text FROM files WHERE id='{}'",
                world.file
            ));

            let child = archive_worker_child(fx, &archive_dir, &archive_key_file, Some(point));
            assert!(
                !child.status.success(),
                "crash point {point} was not reached; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&child.stdout),
                String::from_utf8_lossy(&child.stderr),
            );
            assert_aborted(child.status, &String::from_utf8_lossy(&child.stderr));
            assert_eq!(
                fx.text(&format!(
                    "SELECT tier_state FROM blobs WHERE id='{old_blob}'"
                )),
                state,
                "point={point}"
            );
            assert_eq!(
                fx.scalar(&format!(
                    "SELECT archive_local_delete_pending::int::bigint FROM blobs WHERE id='{old_blob}'"
                )),
                if delete_pending { 1 } else { 0 },
                "wrong delete-pending state at {point}"
            );
            assert_eq!(
                fx.scalar(&format!(
                    "SELECT (archive_object_key IS NOT NULL)::int::bigint FROM blobs WHERE id='{old_blob}'"
                )),
                1,
                "durable intent missing at {point}"
            );
            assert_eq!(
                files_below(&fx.storage.join("objects")).len(),
                local_count,
                "local versions must survive {point}"
            );
            assert_local_version_bytes(fx, &head_version, &pattern(1024 * 1024 + 7, 23));
            assert_eq!(
                files_below(&archive_dir.join("objects")).len(),
                if published { 1 } else { 0 },
                "unexpected published archive object at {point}"
            );

            // A pre-commit crash needs the stale-claim lease to expire. A
            // committed archive uses its durable pending-delete queue directly.
            if state == "archiving" {
                fx.exec(&format!(
                    "UPDATE blobs SET archive_claimed_at=now()-interval '16 minutes' WHERE id='{old_blob}'"
                ));
            }
            let resumed = archive_worker_child(fx, &archive_dir, &archive_key_file, None);
            assert!(
                resumed.status.success(),
                "{}",
                String::from_utf8_lossy(&resumed.stderr)
            );
            assert_eq!(
                fx.text(&format!(
                    "SELECT tier_state FROM blobs WHERE id='{old_blob}'"
                )),
                "archived",
                "recovery failed after {point}"
            );
            assert_eq!(
                fx.scalar(&format!(
                    "SELECT archive_local_delete_pending::int::bigint FROM blobs WHERE id='{old_blob}'"
                )),
                0,
                "cleanup state remains pending after {point}"
            );
            assert_eq!(
                files_below(&fx.storage.join("objects")).len(),
                1,
                "new head must remain local after {point}"
            );
            assert_archive_bytes(fx, &world.v1, &archive_dir, &world.v1_data);
            assert_local_version_bytes(fx, &head_version, &pattern(1024 * 1024 + 7, 23));
        }
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn chunk_archive_process_crashes_resume_without_losing_version_bytes() {
        for (point, local_survives) in [
            ("archive-chunk-db-committed", true),
            ("archive-chunk-local-deleted", false),
        ] {
            let world =
                World::new_chunked_with_first_size(12 * 1024 * 1024, |fixture, api, file| {
                    let version = tag(&fixture.text(&format!(
                        "SELECT head_version_id::text FROM files WHERE id='{file}'"
                    )));
                    let mut data = pattern(12 * 1024 * 1024, 23);
                    let start = data.len() / 2;
                    let changed = data.len() / 100;
                    data[start..start + changed].fill(0x6b);
                    api.publish(file, &data, ("If-Match", version.as_str()));
                });
            let fx = &world.fx;
            assert_eq!(
                fx.text(&format!("SELECT layout FROM blobs WHERE id=(SELECT blob_id FROM file_versions WHERE id='{}')", world.v1)),
                "chunked"
            );
            let archive_dir = fx.root.join(format!("chunk-archive-{}", unique()));
            std::fs::create_dir_all(&archive_dir).unwrap();
            let archive_key_file = fx.root.join(format!("chunk-archive-key-{}", unique()));
            write_archive_test_key(&archive_key_file);
            let old_only: (String, String) = fx.rt.block_on(async {
                sqlx::query_as(
                    "SELECT c.id::text,c.object_key FROM chunks c \
                     JOIN version_chunks old ON old.chunk_id=c.id \
                     WHERE old.version_id=$1 AND NOT EXISTS ( \
                       SELECT 1 FROM version_chunks head JOIN files f ON f.head_version_id=head.version_id \
                       WHERE head.chunk_id=c.id AND f.id=old.file_id) \
                     ORDER BY old.ordinal LIMIT 1",
                )
                .bind(uuid::Uuid::parse_str(&world.v1).unwrap())
                .fetch_one(&fx.pool)
                .await
                .unwrap()
            });

            let child = archive_worker_child(fx, &archive_dir, &archive_key_file, Some(point));
            assert_aborted(child.status, &String::from_utf8_lossy(&child.stderr));
            let state: (String, bool, Option<String>) = fx.rt.block_on(async {
                sqlx::query_as(
                    "SELECT storage_tier,archive_local_delete_pending,archive_object_key FROM chunks WHERE id=$1",
                )
                .bind(uuid::Uuid::parse_str(&old_only.0).unwrap())
                .fetch_one(&fx.pool)
                .await
                .unwrap()
            });
            assert_eq!(state.0, "archive", "{point}");
            assert!(state.1, "durable local-delete retry missing at {point}");
            assert!(
                state.2.is_some(),
                "archive object intent missing at {point}"
            );
            let local = fx.rt.block_on(LocalStore::open(&fx.storage)).unwrap();
            let local_key = ObjectKey::parse(&old_only.1).unwrap();
            assert_eq!(
                fx.rt.block_on(local.head(&local_key)).is_ok(),
                local_survives,
                "wrong local copy state at {point}"
            );

            let resumed = archive_worker_child(fx, &archive_dir, &archive_key_file, None);
            assert!(
                resumed.status.success(),
                "{}",
                String::from_utf8_lossy(&resumed.stderr)
            );
            let pending: i64 = fx.rt.block_on(async {
                sqlx::query_scalar(
                    "SELECT count(*) FROM chunks WHERE storage_tier='archive' AND archive_local_delete_pending",
                )
                .fetch_one(&fx.pool)
                .await
                .unwrap()
            });
            assert_eq!(pending, 0);
            let mut expected = pattern(12 * 1024 * 1024, 23);
            let start = expected.len() / 2;
            let changed = expected.len() / 100;
            expected[start..start + changed].fill(0x6b);
            let head_version = fx.text(&format!(
                "SELECT head_version_id::text FROM files WHERE id='{}'",
                world.file
            ));
            assert_chunked_version_bytes(fx, &world.v1, &archive_dir, &world.v1_data);
            assert_chunked_version_bytes(fx, &head_version, &archive_dir, &expected);
        }
    }

    #[test]
    #[ignore = "requires PostgreSQL: cargo xtask db-test"]
    fn archive_recall_process_crashes_resume_without_releasing_archive_object() {
        for point in [
            "archive-recall-claimed",
            "archive-recall-download-started",
            "archive-recall-download-complete",
            "archive-recall-local-written",
            "archive-recall-verified",
            "archive-recall-committed",
        ] {
            let world = World::new_with_first_size(256 * 1024 + 7, |fixture, api, file| {
                let second_data = pattern(1024 * 1024 + 7, 29);
                let version = tag(&fixture.text(&format!(
                    "SELECT head_version_id::text FROM files WHERE id='{file}'"
                )));
                api.publish(file, &second_data, ("If-Match", version.as_str()));
            });
            let fx = &world.fx;
            let archive_dir = fx.root.join(format!("recall-archive-{}", unique()));
            std::fs::create_dir_all(&archive_dir).unwrap();
            let archive_key_file = fx.root.join(format!("recall-key-{}", unique()));
            write_archive_test_key(&archive_key_file);
            let archived = archive_worker_child(fx, &archive_dir, &archive_key_file, None);
            assert!(
                archived.status.success(),
                "{}",
                String::from_utf8_lossy(&archived.stderr)
            );
            let old_blob = fx.text(&format!(
                "SELECT blob_id::text FROM file_versions WHERE id='{}'",
                world.v1
            ));
            let head_version = fx.text(&format!(
                "SELECT head_version_id::text FROM files WHERE id='{}'",
                world.file
            ));
            assert_eq!(
                fx.text(&format!(
                    "SELECT tier_state FROM blobs WHERE id='{old_blob}'"
                )),
                "archived"
            );
            assert_eq!(files_below(&archive_dir.join("objects")).len(), 1);
            assert!(!files_below(&fx.storage.join("objects")).is_empty());

            let (status, log) =
                fx.archive_recall_process(&archive_dir, &archive_key_file, Some(point));
            assert_aborted(status, &log);
            let state = fx.text(&format!(
                "SELECT tier_state FROM blobs WHERE id='{old_blob}'"
            ));
            assert_eq!(
                state,
                if point == "archive-recall-committed" {
                    "local"
                } else {
                    "recalling"
                },
                "{point}"
            );
            if state == "recalling" {
                fx.exec(&format!(
                    "UPDATE blobs SET archive_claimed_at=now()-interval '16 minutes' WHERE id='{old_blob}'"
                ));
            }

            let (status, log) = fx.archive_recall_process(&archive_dir, &archive_key_file, None);
            assert!(status.success(), "{log}");
            assert_eq!(
                fx.text(&format!(
                    "SELECT tier_state FROM blobs WHERE id='{old_blob}'"
                )),
                "local",
                "recovery failed after {point}"
            );
            assert_local_version_bytes(fx, &world.v1, &world.v1_data);
            assert_local_version_bytes(fx, &head_version, &pattern(1024 * 1024 + 7, 29));
            assert_eq!(
                files_below(&archive_dir.join("objects")).len(),
                1,
                "operator recall must retain archive object after {point}"
            );
        }
    }

    /// Child test process: one real worker run, armed to abort at a named
    /// archive transport boundary. No database setup occurs in the child.
    #[test]
    fn archive_worker_crash_child() {
        if std::env::var_os("CC_FAULT_ARCHIVE_CHILD").is_none() {
            return;
        }
        craftcloud_storage::arm_crash_point_from_env().unwrap();
        let database_file = std::env::var("CC_FAULT_ARCHIVE_DB_FILE").unwrap();
        let local_dir = std::env::var("CC_FAULT_ARCHIVE_STORAGE_DIR").unwrap();
        let archive_dir = std::env::var("CC_FAULT_ARCHIVE_LOCAL_DIR").unwrap();
        let archive_key_file = std::env::var("CC_FAULT_ARCHIVE_KEY_FILE").unwrap();
        let database_url = std::fs::read_to_string(database_file).unwrap();
        let key_text = std::fs::read_to_string(archive_key_file).unwrap();
        let key = base64::engine::general_purpose::STANDARD
            .decode(key_text.trim())
            .unwrap();
        let key: [u8; 32] = key.try_into().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let pool = craftcloud_db::connect(database_url.trim(), 5, Duration::from_secs(20))
                .await
                .unwrap();
            let local = LocalStore::open(std::path::Path::new(&local_dir))
                .await
                .unwrap();
            let archive = LocalStore::open(std::path::Path::new(&archive_dir))
                .await
                .unwrap();
            let crypto = craftcloud_storage::ArchiveCrypto::from_server_key(&key).unwrap();
            craftcloud_api::archive_worker::archive_batch(
                &pool,
                &local,
                &archive,
                &crypto,
                time::OffsetDateTime::now_utc(),
                Duration::ZERO,
            )
            .await
            .unwrap();
        });
    }
}
