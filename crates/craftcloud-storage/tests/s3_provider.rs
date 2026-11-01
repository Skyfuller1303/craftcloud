use std::path::PathBuf;

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
    S3Config, S3Store, Sha256Digest, WriteSink,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

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
            endpoint,
            region,
            bucket: bucket.clone(),
            prefix: Some(prefix.clone()),
            access_key_id: Zeroizing::new(access_key_id),
            secret_access_key: Zeroizing::new(secret_access_key),
        },
        staging_store,
    )
    .expect("valid local MinIO configuration");

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
    client.delete_bucket().bucket(&bucket).send().await.unwrap();
}
