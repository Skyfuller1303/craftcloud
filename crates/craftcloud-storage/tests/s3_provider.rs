#![allow(clippy::panic)]

use std::{
    io::{Read, Write},
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::ByteStream,
};
use aws_smithy_http_client::{
    Builder,
    tls::{self, rustls_provider::CryptoMode},
};
use craftcloud_storage::{
    ArchiveCrypto, ByteRange, CHUNK_BYTES, LocalStore, ObjectKey, ObjectStore, ReadSource,
    S3Config, S3Store, Sha256Digest, StorageError, WriteSink,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Clone, Copy)]
enum CutDirection {
    ClientToServer,
    ServerToClient,
}

struct CutState {
    direction: CutDirection,
    after_bytes: u64,
    seen_bytes: AtomicU64,
    response_statuses: Mutex<Vec<u16>>,
    armed: AtomicBool,
    triggered: AtomicBool,
}

struct AbortProxy {
    endpoint: String,
    stopped: Arc<AtomicBool>,
    state: Arc<CutState>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl AbortProxy {
    fn new(target: SocketAddr, base_path: &str, direction: CutDirection, after_bytes: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .unwrap_or_else(|error| panic!("bind local S3 proxy: {error}"));
        listener
            .set_nonblocking(true)
            .unwrap_or_else(|error| panic!("configure local S3 proxy: {error}"));
        let address = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("read local S3 proxy address: {error}"));
        let stopped = Arc::new(AtomicBool::new(false));
        let state = Arc::new(CutState {
            direction,
            after_bytes,
            seen_bytes: AtomicU64::new(0),
            response_statuses: Mutex::new(Vec::new()),
            armed: AtomicBool::new(true),
            triggered: AtomicBool::new(false),
        });
        let accept_stopped = Arc::clone(&stopped);
        let accept_state = Arc::clone(&state);
        let accept_thread = thread::spawn(move || {
            while !accept_stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let state = Arc::clone(&accept_state);
                        thread::spawn(move || proxy_connection(client, target, state));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            endpoint: format!("http://127.0.0.1:{}{}", address.port(), base_path),
            stopped,
            state,
            accept_thread: Some(accept_thread),
        }
    }

    fn was_cut(&self) -> bool {
        self.state.triggered.load(Ordering::Acquire)
    }

    fn bytes_seen(&self) -> u64 {
        self.state.seen_bytes.load(Ordering::Acquire)
    }

    fn response_statuses(&self) -> Vec<u16> {
        self.state
            .response_statuses
            .lock()
            .map(|statuses| statuses.clone())
            .unwrap_or_default()
    }
}

impl Drop for AbortProxy {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
    }
}

fn proxy_connection(client: TcpStream, target: SocketAddr, state: Arc<CutState>) {
    let Ok(upstream) = TcpStream::connect(target) else {
        return;
    };
    let _ = client.set_nodelay(true);
    let _ = upstream.set_nodelay(true);
    let (Ok(client_read), Ok(client_write), Ok(client_stop)) =
        (client.try_clone(), client.try_clone(), client.try_clone())
    else {
        return;
    };
    let (Ok(upstream_read), Ok(upstream_write), Ok(upstream_stop)) = (
        upstream.try_clone(),
        upstream.try_clone(),
        upstream.try_clone(),
    ) else {
        return;
    };
    let forward_state = Arc::clone(&state);
    thread::spawn(move || {
        relay_direction(
            client_read,
            upstream_write,
            client_stop,
            upstream_stop,
            forward_state,
            CutDirection::ClientToServer,
        );
    });
    thread::spawn(move || {
        relay_direction(
            upstream_read,
            client_write,
            client,
            upstream,
            state,
            CutDirection::ServerToClient,
        );
    });
}

fn relay_direction(
    mut source: TcpStream,
    mut destination: TcpStream,
    client_stop: TcpStream,
    upstream_stop: TcpStream,
    state: Arc<CutState>,
    direction: CutDirection,
) {
    let mut buffer = [0_u8; 8192];
    loop {
        let Ok(count) = source.read(&mut buffer) else {
            let _ = destination.shutdown(Shutdown::Write);
            return;
        };
        if count == 0 {
            let _ = destination.shutdown(Shutdown::Write);
            return;
        }
        if matches!(direction, CutDirection::ServerToClient)
            && buffer.starts_with(b"HTTP/")
            && let Some(status) = buffer
                .get(9..12)
                .and_then(|status| std::str::from_utf8(status).ok())
                .and_then(|status| status.parse::<u64>().ok())
            && let Ok(mut statuses) = state.response_statuses.lock()
            && statuses.len() < 8
        {
            statuses.push(status as u16);
        }
        if matches_direction(state.direction, direction) && state.armed.load(Ordering::Acquire) {
            let previous = state.seen_bytes.fetch_add(count as u64, Ordering::AcqRel);
            let remaining = state.after_bytes.saturating_sub(previous);
            if (count as u64) >= remaining {
                if remaining > 0 {
                    let Some(bytes) = buffer.get(..remaining as usize) else {
                        return;
                    };
                    if destination.write_all(bytes).is_err() {
                        return;
                    }
                }
                if state.armed.swap(false, Ordering::AcqRel) {
                    state.triggered.store(true, Ordering::Release);
                    let _ = client_stop.shutdown(Shutdown::Both);
                    let _ = upstream_stop.shutdown(Shutdown::Both);
                    return;
                }
            }
        }
        let Some(bytes) = buffer.get(..count) else {
            return;
        };
        if destination.write_all(bytes).is_err() {
            return;
        }
    }
}

fn matches_direction(left: CutDirection, right: CutDirection) -> bool {
    matches!(
        (left, right),
        (CutDirection::ClientToServer, CutDirection::ClientToServer)
            | (CutDirection::ServerToClient, CutDirection::ServerToClient)
    )
}

fn loopback_target(endpoint: &str) -> (SocketAddr, String) {
    let url =
        url::Url::parse(endpoint).unwrap_or_else(|error| panic!("parse MinIO endpoint: {error}"));
    assert_eq!(
        url.scheme(),
        "http",
        "the socket-cut proxy requires loopback HTTP"
    );
    let host = url
        .host_str()
        .unwrap_or_else(|| panic!("MinIO endpoint has no host"));
    assert!(
        host == "localhost"
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback()),
        "the socket-cut proxy only targets loopback MinIO"
    );
    let port = url
        .port_or_known_default()
        .unwrap_or_else(|| panic!("MinIO endpoint has no port"));
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let target = (host, port)
        .to_socket_addrs()
        .unwrap_or_else(|error| panic!("resolve local MinIO endpoint: {error}"))
        .next()
        .unwrap_or_else(|| panic!("local MinIO endpoint resolved to no addresses"));
    (target, url.path().trim_end_matches('/').to_owned())
}

async fn write_bytes(
    store: &S3Store,
    key: &ObjectKey,
    bytes: &[u8],
    digest: &Sha256Digest,
) -> Result<ObjectKey, StorageError> {
    let mut temp = store
        .begin_write_as(key.clone(), bytes.len() as u64)
        .await?;
    for chunk in bytes.chunks(60_001) {
        temp.write(chunk).await?;
    }
    store.finalize(temp, bytes.len() as u64, digest).await
}

fn proxy_store(
    endpoint: String,
    region: &str,
    bucket: &str,
    prefix: &str,
    access_key_id: &str,
    secret_access_key: &str,
    staging: LocalStore,
) -> S3Store {
    S3Store::connect(
        S3Config {
            endpoint,
            region: region.to_owned(),
            bucket: bucket.to_owned(),
            prefix: Some(prefix.to_owned()),
            access_key_id: Zeroizing::new(access_key_id.to_owned()),
            secret_access_key: Zeroizing::new(secret_access_key.to_owned()),
        },
        staging,
    )
    .unwrap_or_else(|error| panic!("invalid loopback proxy S3 configuration: {error}"))
}

fn required(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::test]
async fn s3_provider_contract_streams_only_archive_ciphertext() {
    let (Some(endpoint), Some(region), Some(access_key_id), Some(secret_access_key), Some(staging)) = (
        required("CRAFTCLOUD_S3_TEST_ENDPOINT"),
        required("CRAFTCLOUD_S3_TEST_REGION"),
        required("CRAFTCLOUD_S3_TEST_ACCESS_KEY_ID"),
        required("CRAFTCLOUD_S3_TEST_SECRET_ACCESS_KEY"),
        required("CRAFTCLOUD_S3_TEST_STAGING"),
    ) else {
        eprintln!("S3 provider test skipped: CRAFTCLOUD_S3_TEST_* is incomplete");
        return;
    };
    let staging_path = PathBuf::from(staging);
    std::fs::create_dir_all(&staging_path).unwrap();
    let staging_store = LocalStore::open(&staging_path).await.unwrap();
    let bucket = format!("cc-ac12-{}", Uuid::new_v4().simple());
    let client = Client::from_conf(
        aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(region.clone()))
            .credentials_provider(Credentials::new(
                access_key_id.as_str(),
                secret_access_key.as_str(),
                None,
                None,
                "CraftCloudTest",
            ))
            .endpoint_url(endpoint.clone())
            .force_path_style(true)
            .http_client(
                Builder::new()
                    .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
                    .build_https(),
            )
            .build(),
    );
    client.create_bucket().bucket(&bucket).send().await.unwrap();
    let prefix = format!("ac12-test-{}", Uuid::new_v4().simple());
    let store = S3Store::connect(
        S3Config {
            endpoint: endpoint.clone(),
            region: region.clone(),
            bucket: bucket.clone(),
            prefix: Some(prefix.clone()),
            access_key_id: Zeroizing::new(access_key_id.clone()),
            secret_access_key: Zeroizing::new(secret_access_key.clone()),
        },
        staging_store,
    )
    .expect("valid local MinIO configuration");

    let missing_key = ObjectKey::generate();
    assert!(matches!(
        store.head(&missing_key).await,
        Err(StorageError::NotFound)
    ));
    assert!(matches!(
        store.open_read(&missing_key, None).await,
        Err(StorageError::NotFound)
    ));
    let missing_temp = store
        .begin_write_as(missing_key, 1)
        .await
        .expect("HTTP 404 for a missing object allows a reserved upload");
    store.abort(missing_temp).await;

    let mut plaintext = (0..(8 * 1024 * 1024 + 123))
        .map(|index| ((index * 197 + 89) % 256) as u8)
        .collect::<Vec<_>>();
    let marker = b"archive plaintext secret";
    plaintext[10_000..10_000 + marker.len()].copy_from_slice(marker);
    let crypto = ArchiveCrypto::from_server_key(&[0x92; 32]).unwrap();
    let blob_id = Uuid::new_v4();
    let (envelope, data_key) = crypto
        .create_data_key(blob_id, plaintext.len() as u64)
        .unwrap();
    let mut raw_ciphertext = Vec::new();
    for (index, block) in plaintext.chunks(CHUNK_BYTES).enumerate() {
        raw_ciphertext.extend_from_slice(
            &data_key
                .encrypt_block(&envelope, index as u64, block)
                .unwrap(),
        );
    }
    assert!(
        !raw_ciphertext
            .windows(marker.len())
            .any(|window| window == marker)
    );
    let digest: Sha256Digest = Sha256::digest(&raw_ciphertext).into();
    let mut temp = store
        .begin_write(raw_ciphertext.len() as u64)
        .await
        .unwrap();
    for chunk in raw_ciphertext.chunks(60_001) {
        temp.write(chunk).await.unwrap();
    }
    let expected_key = temp.object_key().unwrap();
    let key = store
        .finalize(temp, raw_ciphertext.len() as u64, &digest)
        .await
        .unwrap();
    assert_eq!(key, expected_key);
    assert_eq!(
        store.head(&key).await.unwrap().len,
        raw_ciphertext.len() as u64
    );
    store.verify(&key, &digest).await.unwrap();

    let mut range = store
        .open_read(
            &key,
            Some(ByteRange {
                start: 63_999,
                end: Some(131_077),
            }),
        )
        .await
        .unwrap();
    let mut actual_range = Vec::new();
    while let Some(chunk) = range.next_chunk().await.unwrap() {
        assert!(chunk.len() <= CHUNK_BYTES);
        actual_range.extend_from_slice(&chunk);
    }
    assert_eq!(actual_range, raw_ciphertext[63_999..131_077]);
    drop(range);

    let mut raw = store.open_read(&key, None).await.unwrap();
    let mut actual = Vec::new();
    while let Some(chunk) = raw.next_chunk().await.unwrap() {
        assert!(chunk.len() <= CHUNK_BYTES);
        actual.extend_from_slice(&chunk);
    }
    drop(raw);
    assert_eq!(actual, raw_ciphertext);
    store.delete(&key).await.unwrap();

    let collision_key = ObjectKey::generate();
    let collision_remote_key = format!("{prefix}/{collision_key}");
    let existing_bytes = b"published bytes stay unchanged";
    let mut collision_temp = store
        .begin_write_as(collision_key.clone(), raw_ciphertext.len() as u64)
        .await
        .unwrap();
    for chunk in raw_ciphertext.chunks(60_001) {
        collision_temp.write(chunk).await.unwrap();
    }
    client
        .put_object()
        .bucket(&bucket)
        .key(&collision_remote_key)
        .body(ByteStream::from(existing_bytes.to_vec()))
        .send()
        .await
        .unwrap();
    assert!(matches!(
        store
            .finalize(collision_temp, raw_ciphertext.len() as u64, &digest)
            .await,
        Err(craftcloud_storage::StorageError::AlreadyExists)
    ));
    let mut existing = store.open_read(&collision_key, None).await.unwrap();
    let mut actual_existing = Vec::new();
    while let Some(chunk) = existing.next_chunk().await.unwrap() {
        actual_existing.extend_from_slice(&chunk);
    }
    assert_eq!(actual_existing, existing_bytes);
    drop(existing);
    store.delete(&collision_key).await.unwrap();

    let orphan_key = ObjectKey::generate();
    let stale_path = staging_path
        .join("tmp")
        .join(format!("{}.part", orphan_key.as_str()));
    std::fs::write(&stale_path, b"ciphertext staging after crash").unwrap();
    let remote_key = format!("{prefix}/{orphan_key}");
    let upload_id = client
        .create_multipart_upload()
        .bucket(&bucket)
        .key(&remote_key)
        .send()
        .await
        .unwrap()
        .upload_id()
        .unwrap()
        .to_owned();
    assert!(
        store
            .list_temp(None, 10)
            .await
            .unwrap()
            .iter()
            .any(|temp| temp.id == orphan_key.as_str())
    );
    store.remove_temp(orphan_key.as_str()).await.unwrap();
    assert!(!stale_path.exists());
    assert!(
        client
            .list_parts()
            .bucket(&bucket)
            .key(&remote_key)
            .upload_id(&upload_id)
            .send()
            .await
            .is_err()
    );

    // A local TCP relay cuts the actual S3 socket once, after enough bytes
    // have crossed to land inside the multipart body. The same immutable key
    // is then retried through the recovered connection.
    let (minio_address, base_path) = loopback_target(&endpoint);
    let upload_proxy = AbortProxy::new(
        minio_address,
        &base_path,
        CutDirection::ClientToServer,
        64 * 1024,
    );
    let upload_staging_path = staging_path.join("socket-abort-upload");
    std::fs::create_dir_all(&upload_staging_path).unwrap();
    let upload_staging = LocalStore::open(&upload_staging_path).await.unwrap();
    let upload_store = proxy_store(
        upload_proxy.endpoint.clone(),
        &region,
        &bucket,
        &prefix,
        &access_key_id,
        &secret_access_key,
        upload_staging,
    );
    let recovery_key = ObjectKey::generate();
    let interrupted_upload =
        write_bytes(&upload_store, &recovery_key, &raw_ciphertext, &digest).await;
    assert!(
        interrupted_upload.is_err(),
        "interrupted upload unexpectedly succeeded"
    );
    assert!(
        upload_proxy.was_cut(),
        "upload proxy did not cut the S3 socket (forwarded {} client bytes; HTTP statuses {:?}; upload error: {:?})",
        upload_proxy.bytes_seen(),
        upload_proxy.response_statuses(),
        interrupted_upload.unwrap_err()
    );
    assert!(
        client
            .head_object()
            .bucket(&bucket)
            .key(format!("{prefix}/{recovery_key}"))
            .send()
            .await
            .is_err()
    );
    assert_eq!(
        write_bytes(&upload_store, &recovery_key, &raw_ciphertext, &digest)
            .await
            .unwrap(),
        recovery_key
    );
    upload_store.verify(&recovery_key, &digest).await.unwrap();

    // The reverse-direction cut truncates a real GET response body. Reopening
    // the same immutable object yields the full ciphertext again.
    let download_proxy = AbortProxy::new(
        minio_address,
        &base_path,
        CutDirection::ServerToClient,
        64 * 1024,
    );
    let download_staging_path = staging_path.join("socket-abort-download");
    std::fs::create_dir_all(&download_staging_path).unwrap();
    let download_staging = LocalStore::open(&download_staging_path).await.unwrap();
    let download_store = proxy_store(
        download_proxy.endpoint.clone(),
        &region,
        &bucket,
        &prefix,
        &access_key_id,
        &secret_access_key,
        download_staging,
    );
    let mut interrupted = download_store.open_read(&recovery_key, None).await.unwrap();
    let mut received_before_cut = Vec::new();
    let mut read_failed = false;
    loop {
        match interrupted.next_chunk().await {
            Ok(Some(chunk)) => received_before_cut.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(_) => {
                read_failed = true;
                break;
            }
        }
    }
    drop(interrupted);
    assert!(
        download_proxy.was_cut(),
        "download proxy did not cut the S3 socket"
    );
    assert!(
        read_failed,
        "truncated S3 response was reported as complete"
    );
    assert!(!received_before_cut.is_empty());
    assert!(received_before_cut.len() < raw_ciphertext.len());
    let mut recovered = download_store.open_read(&recovery_key, None).await.unwrap();
    let mut recovered_bytes = Vec::new();
    while let Some(chunk) = recovered.next_chunk().await.unwrap() {
        recovered_bytes.extend_from_slice(&chunk);
    }
    assert_eq!(recovered_bytes, raw_ciphertext);
    drop(recovered);
    download_store.delete(&recovery_key).await.unwrap();
    client.delete_bucket().bucket(&bucket).send().await.unwrap();
}
