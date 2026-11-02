//! S3-compatible archive adapter. Publication is conditional at the provider.

use std::{fmt, io, pin::Pin, time::Duration};

use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Credentials, Region},
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use aws_smithy_http_client::{
    Builder,
    tls::{self, rustls_provider::CryptoMode},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use url::Url;
use zeroize::Zeroizing;

use crate::{
    ByteRange, CHUNK_BYTES, LocalStore, ObjectHead, ObjectInfo, ObjectKey, ObjectStore, ReadSource,
    Sha256Digest, StorageError, StoreSetupError, TempInfo, TempObject, WriteSink,
    rate_limit::BandwidthLimit,
};

const S3_PART_BYTES: usize = 8 * 1024 * 1024;
const S3_MAX_PARTS: u64 = 10_000;
const S3_MAX_BYTES: u64 = S3_PART_BYTES as u64 * S3_MAX_PARTS;

/// S3-compatible archive endpoint configuration.
pub struct S3Config {
    /// HTTPS endpoint. Plain HTTP is allowed only for loopback tests.
    pub endpoint: String,
    /// S3 region.
    pub region: String,
    /// Existing bucket name.
    pub bucket: String,
    /// Optional opaque key prefix containing ASCII letters, digits, `_`, `-`.
    pub prefix: Option<String>,
    /// Access key loaded by the caller from its bounded secret loader.
    pub access_key_id: Zeroizing<String>,
    /// Secret key loaded by the caller from its bounded secret loader.
    pub secret_access_key: Zeroizing<String>,
}

/// S3-compatible object storage. Caller must send archive ciphertext.
pub struct S3Store {
    client: Client,
    bucket: String,
    staging: LocalStore,
    prefix: String,
    bandwidth: BandwidthLimit,
}

/// Local ciphertext staging for an S3 publication.
pub struct S3Temp {
    local: TempObject,
    reserved: bool,
}

#[allow(async_fn_in_trait)]
impl WriteSink for S3Temp {
    fn object_key(&self) -> Result<ObjectKey, StorageError> {
        self.local.object_key()
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        self.local.write(data).await
    }

    fn bytes_written(&self) -> u64 {
        self.local.bytes_written()
    }
}

struct MultipartFailure {
    error: StorageError,
    preserve_staging: bool,
}

impl fmt::Debug for S3Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Store")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl S3Store {
    /// Connects to an S3 endpoint with certificate validation and no redirects.
    pub fn connect(config: S3Config, staging: LocalStore) -> Result<Self, StoreSetupError> {
        let endpoint = Url::parse(&config.endpoint)
            .map_err(|_| StoreSetupError("invalid S3 endpoint".to_owned()))?;
        let http_allowed =
            endpoint.scheme() == "http" && endpoint.host_str().is_some_and(is_loopback_host);
        if endpoint.scheme() != "https" && !http_allowed {
            return Err(StoreSetupError(
                "S3 endpoint must use HTTPS (HTTP is allowed only on loopback)".to_owned(),
            ));
        }
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.host_str().is_none()
            || endpoint_authority_has_userinfo(&config.endpoint)
        {
            return Err(StoreSetupError(
                "S3 endpoint cannot contain credentials, query, or fragment".to_owned(),
            ));
        }
        if config.region.is_empty()
            || !config
                .region
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(StoreSetupError("invalid S3 region".to_owned()));
        }
        if config.bucket.is_empty()
            || !config
                .bucket
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(StoreSetupError("invalid S3 bucket name".to_owned()));
        }
        let prefix = config
            .prefix
            .unwrap_or_default()
            .trim_matches('/')
            .to_owned();
        if !prefix.is_empty() && !prefix.split('/').all(valid_prefix_segment) {
            return Err(StoreSetupError("invalid S3 object prefix".to_owned()));
        }
        let credentials = Credentials::new(
            config.access_key_id.as_str(),
            config.secret_access_key.as_str(),
            None,
            None,
            "CraftCloudArchive",
        );
        let sdk_config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .operation_timeout(Duration::from_secs(60))
                    .operation_attempt_timeout(Duration::from_secs(20))
                    .build(),
            )
            .retry_config(data_transfer_retry_config())
            .region(Region::new(config.region))
            .credentials_provider(credentials)
            .endpoint_url(config.endpoint)
            .force_path_style(true)
            .http_client(
                Builder::new()
                    .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
                    .build_https(),
            )
            .build();
        Ok(Self {
            client: Client::from_conf(sdk_config),
            bucket: config.bucket,
            staging,
            prefix,
            bandwidth: BandwidthLimit::from_mbps(None),
        })
    }

    /// Sets a shared decimal-Mbit/s budget for provider transfer and readback bytes.
    /// Buffered S3 uploads can burst by at most one 8 MiB request body; reads by 64 KiB.
    pub fn set_bandwidth_limit_mbps(&mut self, mbps: Option<u64>) {
        self.bandwidth = BandwidthLimit::from_mbps(mbps);
    }

    fn key(&self, key: &ObjectKey) -> String {
        if self.prefix.is_empty() {
            key.as_str().to_owned()
        } else {
            format!("{}/{key}", self.prefix)
        }
    }

    fn prefix_for_list(&self) -> Option<String> {
        (!self.prefix.is_empty()).then(|| format!("{}/", self.prefix))
    }

    async fn upload_small(&self, key: &str, bytes: Vec<u8>) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .if_none_match("*")
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map(|_| ())
            .map_err(map_s3_publish_error)
    }

    async fn upload_multipart(
        &self,
        key: &str,
        temp: &S3Temp,
        expected_len: u64,
    ) -> Result<(), MultipartFailure> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| MultipartFailure {
                error: map_s3_error(error),
                preserve_staging: true,
            })?;
        let Some(upload_id) = created.upload_id().map(str::to_owned) else {
            return Err(MultipartFailure {
                error: StorageError::Io(io::Error::other(
                    "S3 multipart response omitted upload ID",
                )),
                preserve_staging: true,
            });
        };
        let result = self.upload_parts(key, temp, expected_len, &upload_id).await;
        if let Err(error) = result {
            let preserve_staging = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await
                .is_err();
            return Err(MultipartFailure {
                error,
                preserve_staging,
            });
        }
        Ok(())
    }

    async fn upload_parts(
        &self,
        key: &str,
        temp: &S3Temp,
        expected_len: u64,
        upload_id: &str,
    ) -> Result<(), StorageError> {
        let mut reader = self.staging.open_temp_read(&temp.local).await?;
        let mut parts = Vec::new();
        let mut part_number = 1_i32;
        let mut sent = 0_u64;
        while sent < expected_len {
            if part_number > S3_MAX_PARTS as i32 {
                return Err(StorageError::TooLarge);
            }
            let capacity = usize::try_from((expected_len - sent).min(S3_PART_BYTES as u64))
                .map_err(|_| StorageError::TooLarge)?;
            let mut bytes = Vec::with_capacity(capacity);
            while bytes.len() < capacity {
                let Some(chunk) = reader.next_chunk().await? else {
                    return Err(StorageError::LengthMismatch);
                };
                let take = chunk.len().min(capacity - bytes.len());
                bytes.extend_from_slice(chunk.get(..take).ok_or(StorageError::LengthMismatch)?);
                if take < chunk.len() {
                    return Err(StorageError::LengthMismatch);
                }
            }
            sent = sent
                .checked_add(bytes.len() as u64)
                .ok_or(StorageError::TooLarge)?;
            // S3 SDK accepts buffered request bodies, not a paced socket stream.
            // This bounds transfer bursts to one multipart part (8 MiB).
            self.bandwidth.pace(bytes.len()).await;
            let response = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(bytes))
                .send()
                .await
                .map_err(map_s3_error)?;
            let Some(etag) = response.e_tag() else {
                return Err(StorageError::Io(io::Error::other(
                    "S3 multipart response omitted ETag",
                )));
            };
            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .e_tag(etag)
                    .build(),
            );
            part_number = part_number.checked_add(1).ok_or(StorageError::TooLarge)?;
        }
        if reader.next_chunk().await?.is_some() {
            return Err(StorageError::LengthMismatch);
        }
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(parts))
            .build();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(completed)
            .if_none_match("*")
            .send()
            .await
            .map(|_| ())
            .map_err(map_s3_publish_error)
    }

    async fn abort_multipart_for_key(&self, key: &str) -> Result<bool, StorageError> {
        let mut key_marker: Option<String> = None;
        let mut upload_id_marker: Option<String> = None;
        let mut aborted = false;
        loop {
            let mut request = self
                .client
                .list_multipart_uploads()
                .bucket(&self.bucket)
                .prefix(key)
                .max_uploads(1000);
            if let Some(marker) = key_marker.as_ref() {
                request = request.key_marker(marker);
            }
            if let Some(marker) = upload_id_marker.as_ref() {
                request = request.upload_id_marker(marker);
            }
            let response = request.send().await.map_err(map_s3_error)?;
            for upload in response.uploads() {
                if upload.key() != Some(key) {
                    continue;
                }
                let upload_id = upload.upload_id().ok_or_else(|| {
                    StorageError::Io(io::Error::other("S3 multipart record omitted upload ID"))
                })?;
                self.client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await
                    .map_err(map_s3_error)?;
                aborted = true;
            }
            if response.is_truncated() != Some(true) {
                break;
            }
            key_marker = response.next_key_marker().map(str::to_owned);
            upload_id_marker = response.next_upload_id_marker().map(str::to_owned);
            if key_marker.is_none() {
                return Err(StorageError::LengthMismatch);
            }
        }
        Ok(aborted)
    }
}

fn data_transfer_retry_config() -> aws_sdk_s3::config::retry::RetryConfig {
    // AWS SDK attempts include the initial request. Do not retry a PUT or
    // UploadPart internally: a hidden retry would bypass transfer pacing.
    aws_sdk_s3::config::retry::RetryConfig::standard().with_max_attempts(1)
}

#[allow(async_fn_in_trait)]
impl ObjectStore for S3Store {
    type Temp = S3Temp;
    type Reader = S3Reader;

    async fn begin_write(&self, max_bytes: u64) -> Result<Self::Temp, StorageError> {
        self.staging
            .begin_write(max_bytes.min(S3_MAX_BYTES))
            .await
            .map(|local| S3Temp {
                local,
                reserved: false,
            })
    }

    async fn begin_write_as(
        &self,
        key: ObjectKey,
        max_bytes: u64,
    ) -> Result<Self::Temp, StorageError> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.key(&key))
            .send()
            .await
        {
            Ok(_) => return Err(StorageError::AlreadyExists),
            Err(error) if is_s3_not_found(&error) => {}
            Err(_) => return Err(s3_io_error()),
        }
        self.staging
            .begin_write_as(key, max_bytes.min(S3_MAX_BYTES))
            .await
            .map(|local| S3Temp {
                local,
                reserved: true,
            })
    }

    async fn finalize(
        &self,
        mut temp: Self::Temp,
        expected_len: u64,
        expected_sha256: &Sha256Digest,
    ) -> Result<ObjectKey, StorageError> {
        let key = match temp.object_key() {
            Ok(key) => key,
            Err(_) => {
                self.staging.abort(temp.local).await;
                return Err(StorageError::Aborted);
            }
        };
        let reserved = temp.reserved;
        if let Err(error) = temp.local.prepare_read(expected_len, expected_sha256).await {
            self.staging.abort(temp.local).await;
            return Err(error);
        }
        let remote_key = self.key(&key);
        let (mut result, preserve_staging) = if expected_len > S3_MAX_BYTES {
            (Err(StorageError::TooLarge), false)
        } else if expected_len <= S3_PART_BYTES as u64 {
            let result = match self.staging.open_temp_read(&temp.local).await {
                Ok(mut reader) => {
                    let capacity = match usize::try_from(expected_len) {
                        Ok(capacity) => capacity,
                        Err(_) => {
                            self.staging.abort(temp.local).await;
                            return Err(StorageError::TooLarge);
                        }
                    };
                    let mut bytes = Vec::with_capacity(capacity);
                    let mut read_len = 0_u64;
                    let mut read_error = None;
                    loop {
                        match reader.next_chunk().await {
                            Ok(Some(chunk)) => {
                                read_len = read_len.saturating_add(chunk.len() as u64);
                                bytes.extend_from_slice(&chunk);
                            }
                            Ok(None) => break,
                            Err(error) => {
                                read_error = Some(error);
                                break;
                            }
                        }
                    }
                    match read_error {
                        Some(error) => Err(error),
                        None if read_len != expected_len => Err(StorageError::LengthMismatch),
                        None => {
                            self.bandwidth.pace(bytes.len()).await;
                            self.upload_small(&remote_key, bytes).await
                        }
                    }
                }
                Err(error) => Err(error),
            };
            (result, false)
        } else {
            match self
                .upload_multipart(&remote_key, &temp, expected_len)
                .await
            {
                Ok(()) => (Ok(()), false),
                Err(failure) => (Err(failure.error), failure.preserve_staging),
            }
        };
        if result.is_ok() {
            result = match self.verify(&key, expected_sha256).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    if should_delete_unverified_publication(reserved) {
                        let _ = self
                            .client
                            .delete_object()
                            .bucket(&self.bucket)
                            .key(&remote_key)
                            .send()
                            .await;
                    }
                    Err(error)
                }
            };
        }
        if preserve_staging {
            temp.local.preserve_for_reconciliation();
        } else {
            self.staging.abort(temp.local).await;
        }
        result.map(|()| key)
    }

    async fn abort(&self, temp: Self::Temp) {
        self.staging.abort(temp.local).await;
    }

    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<Self::Reader, StorageError> {
        let object_key = self.key(key);
        let head = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&object_key)
            .send()
            .await
            .map_err(|error| {
                if is_s3_not_found(&error) {
                    StorageError::NotFound
                } else {
                    s3_io_error()
                }
            })?;
        let len = u64::try_from(head.content_length().ok_or(StorageError::LengthMismatch)?)
            .map_err(|_| StorageError::LengthMismatch)?;
        let (start, end) = match range {
            None => (0, len),
            Some(requested) => {
                let end = requested.end.map_or(len, |value| value.min(len));
                if requested.start > len || end < requested.start {
                    return Err(StorageError::RangeNotSatisfiable);
                }
                (requested.start, end)
            }
        };
        let remaining = end
            .checked_sub(start)
            .ok_or(StorageError::RangeNotSatisfiable)?;
        if remaining == 0 {
            return Ok(S3Reader {
                reader: Box::pin(tokio::io::empty()),
                remaining: 0,
                bandwidth: self.bandwidth.clone(),
            });
        }
        let mut request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&object_key);
        if range.is_some() {
            let last = end
                .checked_sub(1)
                .ok_or(StorageError::RangeNotSatisfiable)?;
            request = request.range(format!("bytes={start}-{last}"));
        }
        let response = request.send().await.map_err(|error| {
            if error
                .as_service_error()
                .is_some_and(|service| service.is_no_such_key())
            {
                StorageError::NotFound
            } else {
                s3_io_error()
            }
        })?;
        if response
            .content_length()
            .and_then(|value| u64::try_from(value).ok())
            != Some(remaining)
        {
            return Err(StorageError::LengthMismatch);
        }
        Ok(S3Reader {
            reader: Box::pin(response.body.into_async_read()),
            remaining,
            bandwidth: self.bandwidth.clone(),
        })
    }

    async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, StorageError> {
        let response = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await
            .map_err(|error| {
                if is_s3_not_found(&error) {
                    StorageError::NotFound
                } else {
                    s3_io_error()
                }
            })?;
        Ok(ObjectHead {
            len: u64::try_from(
                response
                    .content_length()
                    .ok_or(StorageError::LengthMismatch)?,
            )
            .map_err(|_| StorageError::LengthMismatch)?,
        })
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.key(key))
            .send()
            .await
            .map(|_| ())
            .map_err(map_s3_error)
    }

    async fn verify(
        &self,
        key: &ObjectKey,
        expected_sha256: &Sha256Digest,
    ) -> Result<(), StorageError> {
        let mut reader = self.open_read(key, None).await?;
        let mut digest = sha2::Sha256::new();
        use sha2::Digest;
        while let Some(chunk) = reader.next_chunk().await? {
            digest.update(&chunk);
        }
        let actual: Sha256Digest = digest.finalize().into();
        if &actual == expected_sha256 {
            Ok(())
        } else {
            Err(StorageError::ChecksumMismatch)
        }
    }

    async fn list_objects(
        &self,
        after: Option<&ObjectKey>,
        limit: usize,
    ) -> Result<Vec<ObjectInfo>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let list_prefix = self.prefix_for_list();
        let mut request = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .max_keys(i32::try_from(limit.min(1000)).unwrap_or(1000));
        if let Some(prefix) = list_prefix.as_ref() {
            request = request.prefix(prefix);
        }
        if let Some(after) = after {
            request = request.start_after(self.key(after));
        }
        let mut objects = Vec::with_capacity(limit.min(1000));
        let mut continuation = None;
        loop {
            let mut page = request.clone();
            if let Some(token) = continuation.take() {
                page = page.continuation_token(token);
            }
            let response = page.send().await.map_err(map_s3_error)?;
            for object in response.contents() {
                let Some(remote_key) = object.key() else {
                    continue;
                };
                let local_key = if self.prefix.is_empty() {
                    remote_key
                } else {
                    let Some(suffix) = remote_key.strip_prefix(&format!("{}/", self.prefix)) else {
                        continue;
                    };
                    suffix
                };
                let Ok(key) = ObjectKey::parse(local_key) else {
                    continue;
                };
                let len = object
                    .size()
                    .and_then(|size| u64::try_from(size).ok())
                    .ok_or(StorageError::LengthMismatch)?;
                objects.push(ObjectInfo {
                    key,
                    len,
                    modified: None,
                });
                if objects.len() >= limit {
                    return Ok(objects);
                }
            }
            continuation = response.next_continuation_token().map(str::to_owned);
            if continuation.is_none() {
                break;
            }
        }
        Ok(objects)
    }

    async fn list_temp(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TempInfo>, StorageError> {
        self.staging.list_temp(after, limit).await
    }

    async fn remove_temp(&self, id: &str) -> Result<(), StorageError> {
        if !crate::key::is_hex32(id) {
            return Err(StorageError::NotFound);
        }
        let key = ObjectKey::parse(id).map_err(|_| StorageError::NotFound)?;
        self.abort_multipart_for_key(&self.key(&key)).await?;
        match self.staging.remove_temp(id).await {
            Ok(()) | Err(StorageError::NotFound) => {}
            Err(error) => return Err(error),
        }
        match self.staging.delete(&key).await {
            Ok(()) | Err(StorageError::NotFound) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// Provider stream reader; memory stays bounded to provider and caller chunks.
pub struct S3Reader {
    reader: Pin<Box<dyn AsyncRead + Send>>,
    remaining: u64,
    bandwidth: BandwidthLimit,
}

#[allow(async_fn_in_trait)]
impl ReadSource for S3Reader {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, StorageError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let wanted = usize::try_from(self.remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| StorageError::LengthMismatch)?;
        self.bandwidth.pace(wanted).await;
        let mut bytes = vec![0; wanted];
        let read = tokio::time::timeout(Duration::from_secs(30), self.reader.read(&mut bytes))
            .await
            .map_err(|_| s3_io_error())?
            .map_err(|_| s3_io_error())?;
        if read == 0 {
            return Err(StorageError::LengthMismatch);
        }
        bytes.truncate(read);
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(Some(bytes))
    }

    fn remaining(&self) -> u64 {
        self.remaining
    }
}

fn map_s3_error<E>(_error: E) -> StorageError {
    s3_io_error()
}

fn is_s3_not_found<E>(error: &aws_sdk_s3::error::SdkError<E>) -> bool {
    error
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 404)
}

fn should_delete_unverified_publication(reserved: bool) -> bool {
    !reserved
}

fn map_s3_publish_error<E>(error: aws_sdk_s3::error::SdkError<E>) -> StorageError {
    if error
        .raw_response()
        .is_some_and(|response| response.status().as_u16() == 412)
    {
        StorageError::AlreadyExists
    } else {
        s3_io_error()
    }
}

fn s3_io_error() -> StorageError {
    StorageError::Io(io::Error::other("S3 archive operation failed"))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "[::1]"
        || host == "::1"
}

fn endpoint_authority_has_userinfo(endpoint: &str) -> bool {
    endpoint
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'))
}

fn valid_prefix_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[cfg(test)]
mod tests {
    use super::{S3Reader, data_transfer_retry_config};
    use crate::{ReadSource, rate_limit::BandwidthLimit};
    use std::time::{Duration, Instant};
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn provider_reader_paces_returned_chunks() {
        let (mut writer, reader) = tokio::io::duplex(128 * 1024);
        tokio::spawn(async move {
            writer.write_all(&vec![7; 128 * 1024]).await.unwrap();
        });
        let mut reader = S3Reader {
            reader: Box::pin(reader),
            remaining: 128 * 1024,
            bandwidth: BandwidthLimit::from_mbps(Some(1)),
        };
        let started = Instant::now();
        while reader.next_chunk().await.unwrap().is_some() {}
        assert!(started.elapsed() >= Duration::from_millis(950));
    }

    #[test]
    fn s3_data_operations_do_not_retry_behind_the_bandwidth_limiter() {
        assert_eq!(data_transfer_retry_config().max_attempts(), 1);
    }

    #[test]
    fn a_persisted_intent_keeps_published_object_for_retry_verification() {
        assert!(super::should_delete_unverified_publication(false));
        assert!(!super::should_delete_unverified_publication(true));
    }
}
