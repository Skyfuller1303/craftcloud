//! Pinned-host-key SFTP archive adapter using Russh.

use std::{fmt, io, path::PathBuf, sync::Arc, time::Duration};

use russh::{
    client,
    keys::{
        PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, known_hosts::known_host_keys_path,
    },
};
use russh_sftp::{
    client::{SftpSession, fs::File as SftpFile},
    protocol::{OpenFlags, StatusCode},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use zeroize::Zeroizing;

use crate::{
    ByteRange, CHUNK_BYTES, LocalStore, ObjectHead, ObjectInfo, ObjectKey, ObjectStore, ReadSource,
    Sha256Digest, StorageError, StoreSetupError, TempInfo, TempObject, WriteSink,
    rate_limit::BandwidthLimit,
};

const DEFAULT_CONNECTIONS: usize = 4;
const MAX_CONNECTIONS: usize = 64;
const MAX_KNOWN_HOSTS_BYTES: u64 = 1024 * 1024;

/// SFTP configuration. Password authentication is not supported.
pub struct SftpConfig {
    /// Remote host name or IP address.
    pub host: String,
    /// Remote SSH port; Storage Box commonly uses 23.
    pub port: u16,
    /// SSH user.
    pub username: String,
    /// Bounded OpenSSH private-key text from the central secret-file loader.
    pub private_key_openssh: Zeroizing<String>,
    /// Required pinned host keys in OpenSSH known_hosts format.
    pub known_hosts_file: PathBuf,
    /// Dedicated remote archive directory.
    pub base_directory: String,
    /// Maximum simultaneous SSH connections; zero selects four.
    pub max_connections: usize,
}

struct SftpIdentity {
    host: String,
    port: u16,
    username: String,
    base_directory: String,
    pinned_keys: Arc<Vec<PublicKey>>,
    private_key: Arc<russh::keys::PrivateKey>,
}

/// SFTP store. Staging must use a separate local store. Caller must pass
/// archive ciphertext; this adapter does not encrypt.
pub struct SftpStore {
    identity: Arc<SftpIdentity>,
    staging: LocalStore,
    connections: Arc<Semaphore>,
    bandwidth: BandwidthLimit,
}

impl fmt::Debug for SftpStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SftpStore")
            .field("host", &self.identity.host)
            .field("port", &self.identity.port)
            .field("base_directory", &self.identity.base_directory)
            .field("private_key", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl SftpStore {
    /// Opens a pinned-key SFTP store. Missing or malformed pins fail closed.
    pub fn connect(config: SftpConfig, staging: LocalStore) -> Result<Self, StoreSetupError> {
        if config.host.is_empty() || config.username.is_empty() || config.port == 0 {
            return Err(StoreSetupError(
                "SFTP host, user, and nonzero port are required".to_owned(),
            ));
        }
        let host_meta = std::fs::symlink_metadata(&config.known_hosts_file)
            .map_err(|_| StoreSetupError("SFTP known-hosts file is unavailable".to_owned()))?;
        if !host_meta.is_file() || host_meta.file_type().is_symlink() {
            return Err(StoreSetupError(
                "SFTP known-hosts path must be a regular file".to_owned(),
            ));
        }
        if host_meta.len() > MAX_KNOWN_HOSTS_BYTES {
            return Err(StoreSetupError(
                "SFTP known-hosts file exceeds 1 MiB".to_owned(),
            ));
        }
        let pinned = known_host_keys_path(&config.host, config.port, &config.known_hosts_file)
            .map_err(|_| StoreSetupError("SFTP known-hosts file could not be parsed".to_owned()))?;
        if pinned.is_empty() {
            return Err(StoreSetupError(
                "SFTP known-hosts file has no matching host key".to_owned(),
            ));
        }
        let private_key = russh::keys::decode_secret_key(config.private_key_openssh.as_str(), None)
            .map_err(|_| StoreSetupError("SFTP private key could not be loaded".to_owned()))?;
        let base_directory = normalize_base_directory(&config.base_directory)?;
        let max_connections = match config.max_connections {
            0 => DEFAULT_CONNECTIONS,
            value if value <= MAX_CONNECTIONS => value,
            _ => {
                return Err(StoreSetupError(
                    "SFTP connection limit exceeds 64".to_owned(),
                ));
            }
        };
        Ok(Self {
            identity: Arc::new(SftpIdentity {
                host: config.host,
                port: config.port,
                username: config.username,
                base_directory,
                pinned_keys: Arc::new(pinned.into_iter().map(|(_, key)| key).collect()),
                private_key: Arc::new(private_key),
            }),
            staging,
            connections: Arc::new(Semaphore::new(max_connections)),
            bandwidth: BandwidthLimit::from_mbps(None),
        })
    }

    /// Sets a shared decimal-Mbit/s budget for provider transfer and readback bytes.
    /// SFTP data operations can burst by at most one 64 KiB chunk.
    pub fn set_bandwidth_limit_mbps(&mut self, mbps: Option<u64>) {
        self.bandwidth = BandwidthLimit::from_mbps(mbps);
    }

    async fn session(&self) -> Result<SftpConnection, StorageError> {
        tokio::time::timeout(Duration::from_secs(30), self.session_inner())
            .await
            .map_err(|_| sftp_io_error())?
    }

    async fn session_inner(&self) -> Result<SftpConnection, StorageError> {
        let permit = self
            .connections
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| sftp_io_error())?;
        let handler = PinnedHost {
            keys: self.identity.pinned_keys.clone(),
        };
        let mut ssh = client::connect(
            Arc::new(client::Config::default()),
            (self.identity.host.as_str(), self.identity.port),
            handler,
        )
        .await
        .map_err(|_| sftp_io_error())?;
        let rsa_hash = ssh
            .best_supported_rsa_hash()
            .await
            .map_err(|_| sftp_io_error())?
            .flatten();
        let auth = ssh
            .authenticate_publickey(
                self.identity.username.clone(),
                PrivateKeyWithHashAlg::new(self.identity.private_key.clone(), rsa_hash),
            )
            .await
            .map_err(|_| sftp_io_error())?;
        if !auth.success() {
            return Err(sftp_io_error());
        }
        let channel = ssh
            .channel_open_session()
            .await
            .map_err(|_| sftp_io_error())?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(|_| sftp_io_error())?;
        let sftp = SftpSession::new(channel.into_stream())
            .await
            .map_err(|_| sftp_io_error())?;
        sftp.set_timeout(30);
        Ok(SftpConnection {
            _ssh: ssh,
            sftp,
            _permit: permit,
        })
    }

    fn object_path(&self, key: &ObjectKey) -> String {
        join_remote_path(&self.identity.base_directory, key.as_str())
    }

    fn temporary_path(&self, id: &str) -> String {
        join_remote_path(&self.identity.base_directory, &format!(".tmp/{id}"))
    }

    fn temporary_directory(&self) -> String {
        join_remote_path(&self.identity.base_directory, ".tmp")
    }
}

struct SftpConnection {
    _ssh: client::Handle<PinnedHost>,
    sftp: SftpSession,
    _permit: OwnedSemaphorePermit,
}

struct PinnedHost {
    keys: Arc<Vec<PublicKey>>,
}

impl client::Handler for PinnedHost {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let presented = key.public_key();
        Ok(self.keys.iter().any(|pinned| pinned == &presented))
    }
}

#[allow(async_fn_in_trait)]
impl ObjectStore for SftpStore {
    type Temp = TempObject;
    type Reader = SftpReader;

    async fn begin_write(&self, max_bytes: u64) -> Result<Self::Temp, StorageError> {
        self.staging.begin_write(max_bytes).await
    }

    async fn begin_write_as(
        &self,
        key: ObjectKey,
        max_bytes: u64,
    ) -> Result<Self::Temp, StorageError> {
        match self.head(&key).await {
            Ok(_) => return Err(StorageError::AlreadyExists),
            Err(StorageError::NotFound) => {}
            Err(error) => return Err(error),
        }
        self.staging.begin_write_as(key, max_bytes).await
    }

    async fn finalize(
        &self,
        temp: Self::Temp,
        expected_len: u64,
        expected_sha256: &Sha256Digest,
    ) -> Result<ObjectKey, StorageError> {
        let key = temp.object_key()?;
        let local_key = self
            .staging
            .finalize_as(temp, key, expected_len, expected_sha256)
            .await?;
        let result = self.publish_local(&local_key, expected_sha256).await;
        let cleanup = self.staging.delete(&local_key).await;
        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(local_key),
            (Ok(()), Err(error)) => Err(error),
            (Err(error), _) => Err(error),
        }
    }

    async fn abort(&self, temp: Self::Temp) {
        self.staging.abort(temp).await;
    }

    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<Self::Reader, StorageError> {
        let connection = self.session().await?;
        let mut file = connection
            .sftp
            .open(self.object_path(key))
            .await
            .map_err(map_sftp_error)?;
        let len = file
            .metadata()
            .await
            .map_err(map_sftp_error)?
            .size
            .ok_or(StorageError::LengthMismatch)?;
        let (start, end) = sftp_range(len, range)?;
        if start > 0 {
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|_| sftp_io_error())?;
        }
        Ok(SftpReader {
            file,
            remaining: end.saturating_sub(start),
            bandwidth: self.bandwidth.clone(),
            _connection: connection,
        })
    }

    async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, StorageError> {
        let connection = self.session().await?;
        let size = connection
            .sftp
            .metadata(self.object_path(key))
            .await
            .map_err(map_sftp_error)?
            .size
            .ok_or(StorageError::LengthMismatch)?;
        Ok(ObjectHead { len: size })
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), StorageError> {
        self.session()
            .await?
            .sftp
            .remove_file(self.object_path(key))
            .await
            .map_err(map_sftp_error)
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
        let connection = self.session().await?;
        let entries = connection
            .sftp
            .read_dir(self.identity.base_directory.clone())
            .await
            .map_err(map_sftp_error)?;
        let mut result = Vec::with_capacity(limit.min(128));
        for entry in entries {
            let name = entry.file_name();
            let Ok(key) = ObjectKey::parse(&name) else {
                continue;
            };
            if after.is_some_and(|marker| key.as_str() <= marker.as_str()) {
                continue;
            }
            let len = entry.metadata().size.ok_or(StorageError::LengthMismatch)?;
            result.push(ObjectInfo {
                key,
                len,
                modified: None,
            });
            result.sort_by(|left, right| left.key.cmp(&right.key));
            if result.len() > limit {
                result.pop();
            }
        }
        Ok(result)
    }

    async fn list_temp(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TempInfo>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.session().await?;
        let entries = match connection.sftp.read_dir(self.temporary_directory()).await {
            Ok(entries) => entries,
            Err(error) if is_no_such_file(&error) => return Ok(Vec::new()),
            Err(error) => return Err(map_sftp_error(error)),
        };
        let mut result = Vec::with_capacity(limit.min(128));
        for entry in entries {
            let id = entry.file_name();
            if !crate::key::is_hex32(&id) || after.is_some_and(|marker| id.as_str() <= marker) {
                continue;
            }
            result.push(TempInfo {
                id,
                len: entry.metadata().size.ok_or(StorageError::LengthMismatch)?,
                modified: None,
            });
            result.sort_by(|left, right| left.id.cmp(&right.id));
            if result.len() > limit {
                result.pop();
            }
        }
        Ok(result)
    }

    async fn remove_temp(&self, id: &str) -> Result<(), StorageError> {
        if !crate::key::is_hex32(id) {
            return Err(StorageError::NotFound);
        }
        let key = ObjectKey::parse(id).map_err(|_| StorageError::NotFound)?;
        let connection = self.session().await?;
        match connection.sftp.remove_file(self.temporary_path(id)).await {
            Ok(()) => {}
            Err(error) if is_no_such_file(&error) => {}
            Err(error) => return Err(map_sftp_error(error)),
        }
        drop(connection);
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

impl SftpStore {
    async fn publish_local(
        &self,
        local_key: &ObjectKey,
        expected_sha256: &Sha256Digest,
    ) -> Result<(), StorageError> {
        let connection = self.session().await?;
        let temp_dir = self.temporary_directory();
        if !connection
            .sftp
            .try_exists(temp_dir.clone())
            .await
            .map_err(map_sftp_error)?
        {
            match connection.sftp.create_dir(temp_dir.clone()).await {
                Ok(()) => {}
                Err(_error)
                    if connection
                        .sftp
                        .try_exists(temp_dir.clone())
                        .await
                        .unwrap_or(false) => {}
                Err(error) => return Err(map_sftp_error(error)),
            }
        }
        let final_path = self.object_path(local_key);
        if connection
            .sftp
            .try_exists(final_path.clone())
            .await
            .map_err(map_sftp_error)?
        {
            return Err(StorageError::AlreadyExists);
        }
        let mut local_reader = self.staging.open_read(local_key, None).await?;
        let temp_id = local_key.as_str();
        let remote_path = self.temporary_path(temp_id);
        let mut remote_file = create_exclusive_temp(&connection.sftp, &temp_dir, temp_id).await?;
        while let Some(chunk) = local_reader.next_chunk().await? {
            self.bandwidth.pace(chunk.len()).await;
            if let Err(error) = remote_file.write_all(&chunk).await {
                let _ = remote_file.close().await;
                return Err(StorageError::Io(io::Error::other(error)));
            }
        }
        remote_file.flush().await.map_err(|_| sftp_io_error())?;
        remote_file.close().await.map_err(|_| sftp_io_error())?;
        verify_remote(
            &connection.sftp,
            &remote_path,
            expected_sha256,
            &self.bandwidth,
        )
        .await?;
        // SFTP v3 RENAME fails when destination exists; do not use posix-rename extension.
        if let Err(error) = connection
            .sftp
            .rename(remote_path.clone(), final_path)
            .await
        {
            return Err(map_sftp_error(error));
        }
        verify_remote(
            &connection.sftp,
            &self.object_path(local_key),
            expected_sha256,
            &self.bandwidth,
        )
        .await?;
        Ok(())
    }
}

/// Bounded remote reader; one connection remains held through the stream.
pub struct SftpReader {
    file: SftpFile,
    remaining: u64,
    bandwidth: BandwidthLimit,
    _connection: SftpConnection,
}

#[allow(async_fn_in_trait)]
impl ReadSource for SftpReader {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, StorageError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let wanted = usize::try_from(self.remaining.min(CHUNK_BYTES as u64))
            .map_err(|_| StorageError::LengthMismatch)?;
        self.bandwidth.pace(wanted).await;
        let mut chunk = vec![0; wanted];
        let read = self
            .file
            .read(&mut chunk)
            .await
            .map_err(|_| sftp_io_error())?;
        if read == 0 {
            return Err(StorageError::LengthMismatch);
        }
        chunk.truncate(read);
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(Some(chunk))
    }

    fn remaining(&self) -> u64 {
        self.remaining
    }
}

async fn create_exclusive_temp(
    sftp: &SftpSession,
    directory: &str,
    id: &str,
) -> Result<SftpFile, StorageError> {
    if !crate::key::is_hex32(id) {
        return Err(StorageError::Aborted);
    }
    let path = join_remote_path(directory, id);
    if sftp
        .try_exists(path.clone())
        .await
        .map_err(map_sftp_error)?
    {
        return Err(StorageError::AlreadyExists);
    }
    sftp.open_with_flags(
        path,
        OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
    )
    .await
    .map_err(map_sftp_error)
}

async fn verify_remote(
    sftp: &SftpSession,
    path: &str,
    expected: &Sha256Digest,
    bandwidth: &BandwidthLimit,
) -> Result<(), StorageError> {
    let mut file = sftp.open(path).await.map_err(map_sftp_error)?;
    let mut digest = sha2::Sha256::new();
    use sha2::Digest;
    let mut count = 0_u64;
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        bandwidth.pace(buffer.len()).await;
        let read = file.read(&mut buffer).await.map_err(|_| sftp_io_error())?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(read as u64)
            .ok_or(StorageError::LengthMismatch)?;
        digest.update(buffer.get(..read).ok_or(StorageError::LengthMismatch)?);
    }
    let metadata = file.metadata().await.map_err(map_sftp_error)?;
    file.close().await.map_err(|_| sftp_io_error())?;
    if metadata.size != Some(count) {
        return Err(StorageError::LengthMismatch);
    }
    let actual: Sha256Digest = digest.finalize().into();
    if &actual == expected {
        Ok(())
    } else {
        Err(StorageError::ChecksumMismatch)
    }
}

fn sftp_range(len: u64, range: Option<ByteRange>) -> Result<(u64, u64), StorageError> {
    match range {
        None => Ok((0, len)),
        Some(requested) => {
            let end = requested.end.map_or(len, |value| value.min(len));
            if requested.start > len || end < requested.start {
                Err(StorageError::RangeNotSatisfiable)
            } else {
                Ok((requested.start, end))
            }
        }
    }
}

fn normalize_base_directory(value: &str) -> Result<String, StoreSetupError> {
    if value.contains('\\')
        || value.contains('\0')
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(StoreSetupError("invalid SFTP base directory".to_owned()));
    }
    let absolute = value.starts_with('/');
    let parts = value
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.iter().any(|part| *part == "." || *part == "..") {
        return Err(StoreSetupError(
            "SFTP base directory cannot contain dot segments".to_owned(),
        ));
    }
    let joined = parts.join("/");
    if absolute {
        Ok(format!("/{joined}"))
    } else {
        Ok(joined)
    }
}

fn join_remote_path(base: &str, child: &str) -> String {
    if base.is_empty() || base == "/" {
        format!("/{child}")
    } else {
        format!("{base}/{child}")
    }
}

fn map_sftp_error(error: russh_sftp::client::error::Error) -> StorageError {
    match error {
        russh_sftp::client::error::Error::Status(status)
            if status.status_code == StatusCode::NoSuchFile =>
        {
            StorageError::NotFound
        }
        _ => sftp_io_error(),
    }
}

fn is_no_such_file(error: &russh_sftp::client::error::Error) -> bool {
    matches!(error, russh_sftp::client::error::Error::Status(status) if status.status_code == StatusCode::NoSuchFile)
}

fn sftp_io_error() -> StorageError {
    StorageError::Io(io::Error::other("SFTP archive operation failed"))
}
