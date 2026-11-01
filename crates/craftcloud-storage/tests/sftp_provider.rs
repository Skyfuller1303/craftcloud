use std::path::PathBuf;

use craftcloud_storage::{
    ArchiveCrypto, ByteRange, LocalStore, ObjectStore, ReadSource, SftpConfig, SftpStore,
    Sha256Digest, WriteSink,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

fn required(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[tokio::test]
async fn sftp_provider_contract_and_pinned_host_rejection() {
    let (Some(host), Some(port), Some(user), Some(private_key), Some(known_hosts), Some(staging)) = (
        required("CRAFTCLOUD_SFTP_TEST_HOST"),
        required("CRAFTCLOUD_SFTP_TEST_PORT"),
        required("CRAFTCLOUD_SFTP_TEST_USER"),
        required("CRAFTCLOUD_SFTP_TEST_PRIVATE_KEY"),
        required("CRAFTCLOUD_SFTP_TEST_KNOWN_HOSTS"),
        required("CRAFTCLOUD_SFTP_TEST_STAGING"),
    ) else {
        eprintln!("SFTP provider test skipped: CRAFTCLOUD_SFTP_TEST_* is incomplete");
        return;
    };
    let (Some(port), Some(wrong_known_hosts)) = (
        port.parse::<u16>().ok(),
        required("CRAFTCLOUD_SFTP_TEST_WRONG_KNOWN_HOSTS"),
    ) else {
        panic!("SFTP test port or mismatched known-hosts path is invalid");
    };
    let staging_path = PathBuf::from(staging);
    std::fs::create_dir_all(&staging_path).unwrap();
    let staging_store = LocalStore::open(&staging_path).await.unwrap();
    let make_store = |known_hosts_file| {
        SftpStore::connect(
            SftpConfig {
                host: host.clone(),
                port,
                username: user.clone(),
                private_key_openssh: Zeroizing::new(private_key.clone()),
                known_hosts_file,
                base_directory: "/upload".to_owned(),
                max_connections: 2,
            },
            staging_store.clone(),
        )
    };

    let rejected = make_store(wrong_known_hosts.into());
    if let Ok(store) = rejected {
        let key = craftcloud_storage::ObjectKey::parse("00000000000000000000000000000000").unwrap();
        assert!(
            store.head(&key).await.is_err(),
            "mismatched host key must fail closed"
        );
    }

    let store = make_store(known_hosts.into()).expect("valid pinned SFTP configuration");
    let mut plaintext = (0..200_123)
        .map(|index| ((index * 197 + 89) % 256) as u8)
        .collect::<Vec<_>>();
    let marker = b"archive plaintext secret";
    plaintext[10_000..10_000 + marker.len()].copy_from_slice(marker);
    let crypto = ArchiveCrypto::from_server_key(&[0x91; 32]).unwrap();
    let blob_id = Uuid::new_v4();
    let (envelope, data_key) = crypto
        .create_data_key(blob_id, plaintext.len() as u64)
        .unwrap();
    let mut raw_ciphertext = Vec::new();
    for (index, block) in plaintext
        .chunks(craftcloud_storage::CHUNK_BYTES)
        .enumerate()
    {
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
    let mut reader = store
        .open_read(
            &key,
            Some(ByteRange {
                start: 63_999,
                end: Some(131_077),
            }),
        )
        .await
        .unwrap();
    let mut range = Vec::new();
    while let Some(chunk) = reader.next_chunk().await.unwrap() {
        assert!(chunk.len() <= craftcloud_storage::CHUNK_BYTES);
        range.extend_from_slice(&chunk);
    }
    assert_eq!(range, raw_ciphertext[63_999..131_077]);
    let mut raw = store.open_read(&key, None).await.unwrap();
    let mut bytes = Vec::new();
    while let Some(chunk) = raw.next_chunk().await.unwrap() {
        bytes.extend_from_slice(&chunk);
    }
    assert_eq!(bytes, raw_ciphertext);
    drop(raw);
    drop(reader);
    assert!(matches!(
        store
            .begin_write_as(key.clone(), raw_ciphertext.len() as u64)
            .await,
        Err(craftcloud_storage::StorageError::AlreadyExists)
    ));
    store.verify(&key, &digest).await.unwrap();
    store.delete(&key).await.unwrap();
}

#[tokio::test]
async fn sftp_reserved_key_recovers_crash_staging_without_deleting_published_object() {
    let (Some(host), Some(port), Some(user), Some(private_key), Some(known_hosts), Some(staging)) = (
        required("CRAFTCLOUD_SFTP_TEST_HOST"),
        required("CRAFTCLOUD_SFTP_TEST_PORT"),
        required("CRAFTCLOUD_SFTP_TEST_USER"),
        required("CRAFTCLOUD_SFTP_TEST_PRIVATE_KEY"),
        required("CRAFTCLOUD_SFTP_TEST_KNOWN_HOSTS"),
        required("CRAFTCLOUD_SFTP_TEST_STAGING"),
    ) else {
        eprintln!("SFTP provider test skipped: CRAFTCLOUD_SFTP_TEST_* is incomplete");
        return;
    };
    let Ok(port) = port.parse::<u16>() else {
        panic!("SFTP test port is invalid");
    };
    let staging_path = PathBuf::from(staging);
    std::fs::create_dir_all(&staging_path).unwrap();
    let staging_store = LocalStore::open(&staging_path).await.unwrap();
    let store = SftpStore::connect(
        SftpConfig {
            host,
            port,
            username: user,
            private_key_openssh: Zeroizing::new(private_key),
            known_hosts_file: known_hosts.into(),
            base_directory: "/upload".to_owned(),
            max_connections: 2,
        },
        staging_store,
    )
    .expect("valid pinned SFTP configuration");

    let key = craftcloud_storage::ObjectKey::generate();
    let stale_path = staging_path
        .join("tmp")
        .join(format!("{}.part", key.as_str()));
    std::fs::write(&stale_path, b"crashed local staging bytes").unwrap();
    store.remove_temp(key.as_str()).await.unwrap();
    assert!(!stale_path.exists());

    let bytes = b"recovered immutable archive bytes";
    let digest: Sha256Digest = Sha256::digest(bytes).into();
    let mut temp = store
        .begin_write_as(key.clone(), bytes.len() as u64)
        .await
        .unwrap();
    assert_eq!(temp.object_key().unwrap(), key);
    temp.write(bytes).await.unwrap();
    let published = store
        .finalize(temp, bytes.len() as u64, &digest)
        .await
        .unwrap();
    assert_eq!(published, key);

    // Temp cleanup must never remove the published object at the same key.
    store.remove_temp(key.as_str()).await.unwrap();
    assert_eq!(store.head(&key).await.unwrap().len, bytes.len() as u64);
    store.verify(&key, &digest).await.unwrap();
    store.delete(&key).await.unwrap();
}
