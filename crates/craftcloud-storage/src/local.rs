//! Local filesystem adapter.
//!
//! Layout below the root: `objects/<2 hex>/<2 hex>/<32 hex>` for published
//! objects and `tmp/<32 hex>.part` for unfinished writes. Both live on the
//! same file system (checked at startup by a create-and-hard-link probe) so
//! that publishing is one atomic hard link that can never overwrite.
//!
//! Safety rules: paths are built only from an [`ObjectKey`] or a validated
//! temporary id; the root is canonicalized at startup and neither the root nor
//! `objects`/`tmp`/shard directories may be symbolic links or reparse points;
//! files are opened with `O_NOFOLLOW` (unix) or `FILE_FLAG_OPEN_REPARSE_POINT`
//! (Windows) and rejected unless they are regular files; new files are created
//! exclusively (`create_new`); published objects are read-only.
//!
//! **Writes use `std::fs::File` on the blocking pool, not `tokio::fs::File`:**
//! tokio defers write errors (they surface on a later call, and `sync_all`
//! does not return them), which could publish a short object. Every write
//! error here surfaces from the call that caused it; before publishing, the
//! length on the handle is compared with the expected length as well.

use std::collections::BinaryHeap;
use std::fs::Metadata;
use std::io::{self, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use sha2::{Digest, Sha256};
use std::io::Write as _;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use uuid::Uuid;

use crate::error::{StorageError, StoreSetupError, map_io_error};
use crate::faults::FaultPlan;
use crate::key::{ObjectKey, is_hex32};
use crate::{
    ByteRange, CHUNK_BYTES, ObjectHead, ObjectInfo, ObjectStore, ReadSource, Sha256Digest,
    TempInfo, WriteSink,
};

#[cfg(unix)]
const NOFOLLOW_FLAGS: i32 = libc::O_NOFOLLOW;
#[cfg(windows)]
const NOFOLLOW_FLAGS: u32 = 0x0020_0000; // FILE_FLAG_OPEN_REPARSE_POINT
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Whether `meta` describes a link or reparse point.
fn is_link(meta: &Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// A plain directory: not a link, not a reparse point.
async fn is_real_dir(path: &Path) -> io::Result<bool> {
    let meta = fs::symlink_metadata(path).await?;
    Ok(meta.is_dir() && !is_link(&meta))
}

/// Identity and epoch stored in the marker file of a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkerInfo {
    /// The store identity.
    pub store_id: Uuid,
    /// How many destructive maintenance runs have started (see the database
    /// binding); a marker ahead of the database means the database is older.
    pub epoch: u64,
}

/// Name of the marker file that identifies a store (see [`LocalStore::read_marker`]).
pub const MARKER_FILE: &str = "craftcloud-store.json";

/// Directory below the root that receives objects removed by reconciliation.
pub const QUARANTINE_DIR: &str = "quarantine";

fn unsafe_tree() -> StorageError {
    StorageError::Io(io::Error::other("unsafe storage tree"))
}

#[derive(Debug)]
struct Layout {
    root: PathBuf,
    objects: PathBuf,
    tmp: PathBuf,
}

impl Layout {
    fn dir_of(&self, key: &ObjectKey) -> PathBuf {
        self.objects.join(key.shard1()).join(key.shard2())
    }

    fn path_of(&self, key: &ObjectKey) -> PathBuf {
        self.dir_of(key).join(key.as_str())
    }

    fn temp_path(&self, id: &str) -> PathBuf {
        self.tmp.join(format!("{id}.part"))
    }
}

/// Object store on a local file system directory.
#[derive(Debug, Clone)]
pub struct LocalStore {
    layout: Arc<Layout>,
    faults: Arc<FaultPlan>,
}

impl LocalStore {
    /// Opens (and prepares) the store below `root`: the directory must exist,
    /// be writable and not be a link; `objects` and `tmp` are created if
    /// missing and must be plain directories; a probe proves that a file can
    /// be created in `tmp` and renamed into `objects` (same file system).
    ///
    /// # Errors
    ///
    /// [`StoreSetupError`] naming the problem.
    pub async fn open(root: &Path) -> Result<Self, StoreSetupError> {
        Self::open_inner(root, Arc::new(FaultPlan::default())).await
    }

    /// Like [`LocalStore::open`] with a fault plan (tests only).
    ///
    /// # Errors
    ///
    /// [`StoreSetupError`] naming the problem.
    #[cfg(any(test, feature = "fault-injection"))]
    pub async fn open_with_faults(
        root: &Path,
        faults: Arc<FaultPlan>,
    ) -> Result<Self, StoreSetupError> {
        Self::open_inner(root, faults).await
    }

    async fn open_inner(root: &Path, faults: Arc<FaultPlan>) -> Result<Self, StoreSetupError> {
        let fail =
            |why: &str| StoreSetupError(format!("storage directory {}: {why}", root.display()));
        if !root.is_absolute() {
            return Err(fail("must be an absolute path"));
        }
        let meta = fs::symlink_metadata(root)
            .await
            .map_err(|e| fail(&format!("cannot be read ({e})")))?;
        if is_link(&meta) {
            return Err(fail("must not be a symbolic link or reparse point"));
        }
        if !meta.is_dir() {
            return Err(fail("is not a directory"));
        }
        let canonical = fs::canonicalize(root)
            .await
            .map_err(|e| fail(&format!("cannot be resolved ({e})")))?;
        let layout = Layout {
            root: canonical.clone(),
            objects: canonical.join("objects"),
            tmp: canonical.join("tmp"),
        };
        for dir in [&layout.objects, &layout.tmp] {
            match fs::create_dir(dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(fail(&format!("cannot create {} ({e})", dir.display()))),
            }
            match is_real_dir(dir).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(fail(&format!(
                        "{} must be a plain directory, not a link",
                        dir.display()
                    )));
                }
                Err(e) => return Err(fail(&format!("cannot check {} ({e})", dir.display()))),
            }
        }
        // Writable and same file system with hard links: create in tmp, link
        // into objects.
        let probe_id = Uuid::new_v4().simple().to_string();
        let probe_tmp = layout.tmp.join(format!("{probe_id}.probe"));
        let probe_dst = layout.objects.join(format!("{probe_id}.probe"));
        let (probe_from, probe_to) = (probe_tmp.clone(), probe_dst.clone());
        let probe = blocking(move || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&probe_from)?;
            file.write_all(b"probe")?;
            file.sync_all()?;
            drop(file);
            std::fs::hard_link(&probe_from, &probe_to)
        })
        .await;
        let _ = fs::remove_file(&probe_tmp).await;
        let _ = fs::remove_file(&probe_dst).await;
        probe.map_err(|e| {
            fail(&format!(
                "is not writable, or objects and tmp are on different file systems, or the file system has no hard links ({e})"
            ))
        })?;
        Ok(Self {
            layout: Arc::new(layout),
            faults,
        })
    }

    /// Creates the shard directories of `key`, refusing links.
    async fn ensure_shards(&self, key: &ObjectKey) -> Result<(), StorageError> {
        let first = self.layout.objects.join(key.shard1());
        let second = first.join(key.shard2());
        for dir in [&first, &second] {
            match fs::create_dir(dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(map_io_error(e)),
            }
            if !is_real_dir(dir).await.map_err(map_io_error)? {
                tracing::error!(path = %dir.display(), "storage shard directory is not a plain directory");
                return Err(unsafe_tree());
            }
        }
        Ok(())
    }

    /// Opens an existing published object for reading without following links.
    async fn open_object(&self, key: &ObjectKey) -> Result<(File, Metadata), StorageError> {
        let dir = self.layout.dir_of(key);
        for check in [self.layout.objects.join(key.shard1()), dir] {
            match is_real_dir(&check).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::error!(path = %check.display(), "storage shard directory is not a plain directory");
                    return Err(unsafe_tree());
                }
                Err(e) => return Err(map_io_error(e)),
            }
        }
        let path = self.layout.path_of(key);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(NOFOLLOW_FLAGS)
            .open(&path)
            .await
            .map_err(|e| {
                // O_NOFOLLOW on a link fails with ELOOP; treat as unsafe, not as missing.
                if e.kind() == io::ErrorKind::NotFound {
                    StorageError::NotFound
                } else {
                    tracing::error!(path = %path.display(), error = %e, "cannot open object");
                    map_io_error(e)
                }
            })?;
        let meta = file.metadata().await.map_err(map_io_error)?;
        if !meta.is_file() || is_link(&meta) {
            tracing::error!(path = %path.display(), "storage object is not a regular file");
            return Err(unsafe_tree());
        }
        Ok((file, meta))
    }
}

/// A streamed write in progress. Dropping it removes the temporary file.
#[derive(Debug)]
pub struct TempObject {
    /// Shared with in-flight blocking writes so that a drop waits for them
    /// (the handle must be closed before the file can be removed on Windows).
    file: Arc<Mutex<Option<std::fs::File>>>,
    path: PathBuf,
    id: String,
    buffer: Vec<u8>,
    hasher: Sha256,
    /// Bytes accepted (buffered or on disk).
    written: u64,
    /// Bytes handed to the file so far.
    flushed: u64,
    limit: u64,
    failed: bool,
    finished: bool,
    faults: Arc<FaultPlan>,
}

fn lock_file(file: &Mutex<Option<std::fs::File>>) -> MutexGuard<'_, Option<std::fs::File>> {
    file.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs blocking file work on the blocking pool.
async fn blocking<T, F>(work: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("blocking task failed")),
    }
}

/// Same, for work that already speaks `StorageError`.
async fn blocking_storage<T, F>(work: F) -> Result<T, StorageError>
where
    F: FnOnce() -> Result<T, StorageError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(_) => Err(StorageError::Io(io::Error::other("blocking task failed"))),
    }
}

/// Writes all of `data`; an armed fault writes only the first `fault_after`
/// bytes (a short write) and then fails like a full device.
fn write_all_checked(
    file: &mut std::fs::File,
    data: &[u8],
    fault_after: Option<usize>,
    faults: &FaultPlan,
) -> Result<(), StorageError> {
    if let Some(keep) = fault_after {
        if let Some(prefix) = data.get(..keep) {
            file.write_all(prefix).map_err(map_io_error)?;
        }
        return Err(faults.take_write_fault());
    }
    file.write_all(data).map_err(map_io_error)
}

impl TempObject {
    /// Identifier of the temporary object (visible in [`ObjectStore::list_temp`]).
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Closes and removes the temporary file; the writer is dead afterwards.
    /// Does nothing to the file once the object was published. Waits for an
    /// in-flight blocking write (bounded by one 64 KiB write).
    fn discard(&mut self) {
        self.failed = true;
        self.buffer.clear();
        if self.finished {
            return;
        }
        self.finished = true;
        remove_temp_file(&self.file, &self.path);
    }

    fn die(&mut self, error: StorageError) -> StorageError {
        tracing::warn!(error = %error, "storage write failed; temporary object removed");
        self.discard();
        error
    }

    /// Writes the buffer to disk; every error surfaces here.
    async fn flush(&mut self) -> Result<(), StorageError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.buffer);
        let len = data.len() as u64;
        // An armed fault inside this flush becomes a short write.
        let fault_after = self
            .faults
            .write_limit()
            .filter(|at| *at < self.flushed + len)
            .map(|at| usize::try_from(at.saturating_sub(self.flushed)).unwrap_or(0));
        let (file, faults) = (Arc::clone(&self.file), Arc::clone(&self.faults));
        let result = blocking_storage(move || {
            let mut guard = lock_file(&file);
            let Some(handle) = guard.as_mut() else {
                return Err(StorageError::Aborted);
            };
            write_all_checked(handle, &data, fault_after, &faults)
        })
        .await;
        if result.is_ok() {
            self.flushed += len;
        }
        result
    }

    async fn push(&mut self, data: &[u8]) -> Result<(), StorageError> {
        for chunk in data.chunks(CHUNK_BYTES) {
            if self.buffer.len() + chunk.len() > CHUNK_BYTES {
                self.flush().await?;
            }
            self.buffer.extend_from_slice(chunk);
            self.hasher.update(chunk);
            self.written += chunk.len() as u64;
        }
        Ok(())
    }
}

impl WriteSink for TempObject {
    async fn write(&mut self, data: &[u8]) -> Result<(), StorageError> {
        if self.failed || self.finished {
            return Err(StorageError::Aborted);
        }
        let len = data.len() as u64;
        if self
            .written
            .checked_add(len)
            .is_none_or(|end| end > self.limit)
        {
            return Err(self.die(StorageError::TooLarge));
        }
        match self.push(data).await {
            Ok(()) => Ok(()),
            Err(error) => Err(self.die(error)),
        }
    }

    fn bytes_written(&self) -> u64 {
        self.written
    }
}

/// Closes the handle (required on Windows) and removes the temporary file.
/// Waits for an in-flight blocking write.
fn remove_temp_file(file: &Mutex<Option<std::fs::File>>, path: &Path) {
    *lock_file(file) = None;
    let _ = std::fs::remove_file(path);
}

impl Drop for TempObject {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Abort without finalize (client disconnect, cancelled task). Inside a
        // runtime the cleanup runs on the blocking pool so that waiting for an
        // in-flight write never stalls an async worker; the file disappears
        // shortly after the drop.
        self.failed = true;
        self.finished = true;
        self.buffer.clear();
        let (file, path) = (Arc::clone(&self.file), self.path.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(move || remove_temp_file(&file, &path));
            }
            Err(_) => remove_temp_file(&file, &path),
        }
    }
}

/// A bounded stream of (a range of) an object.
#[derive(Debug)]
pub struct ObjectReader {
    file: File,
    remaining: u64,
}

impl ReadSource for ObjectReader {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, StorageError> {
        if self.remaining == 0 {
            return Ok(None);
        }
        let want = usize::try_from(self.remaining).map_or(CHUNK_BYTES, |r| r.min(CHUNK_BYTES));
        let mut chunk = vec![0_u8; want];
        let got = self.file.read(&mut chunk).await.map_err(map_io_error)?;
        if got == 0 {
            // The file is shorter than its recorded length: truncated or tampered.
            return Err(StorageError::Io(io::Error::from(
                io::ErrorKind::UnexpectedEof,
            )));
        }
        chunk.truncate(got);
        self.remaining -= got as u64;
        Ok(Some(chunk))
    }

    fn remaining(&self) -> u64 {
        self.remaining
    }
}

#[cfg(unix)]
async fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path).await?.sync_all().await
}

#[cfg(not(unix))]
async fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Publishes `src` under `dst` as a hard link; never overwrites. An existing
/// `dst` is [`StorageError::AlreadyExists`].
pub(crate) fn link_no_overwrite(src: &Path, dst: &Path) -> Result<(), StorageError> {
    match std::fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(StorageError::AlreadyExists),
        Err(e) => Err(map_io_error(e)),
    }
}

#[cfg(test)]
#[allow(clippy::permissions_set_readonly_false)]
fn clear_readonly_sync(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

fn temp_id_ok(id: &str) -> bool {
    is_hex32(id)
}

impl ObjectStore for LocalStore {
    type Temp = TempObject;
    type Reader = ObjectReader;

    async fn begin_write(&self, max_bytes: u64) -> Result<TempObject, StorageError> {
        let id = Uuid::new_v4().simple().to_string();
        let path = self.layout.temp_path(&id);
        // Created inline (one quick syscall, no await point): a cancelled
        // future can then never leave a file that no TempObject owns.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&path).map_err(|e| {
            tracing::error!(path = %path.display(), error = %e, "cannot create temporary object");
            map_io_error(e)
        })?;
        Ok(TempObject {
            file: Arc::new(Mutex::new(Some(file))),
            path,
            id,
            buffer: Vec::with_capacity(CHUNK_BYTES),
            hasher: Sha256::new(),
            written: 0,
            flushed: 0,
            limit: max_bytes,
            failed: false,
            finished: false,
            faults: Arc::clone(&self.faults),
        })
    }

    async fn finalize(
        &self,
        mut temp: TempObject,
        expected_len: u64,
        expected_sha256: &Sha256Digest,
    ) -> Result<ObjectKey, StorageError> {
        if temp.failed || temp.finished {
            return Err(StorageError::Aborted);
        }
        if let Err(error) = temp.flush().await {
            return Err(temp.die(error));
        }
        if temp.written != expected_len {
            return Err(temp.die(StorageError::LengthMismatch));
        }
        let digest: Sha256Digest = temp.hasher.clone().finalize().into();
        if digest != *expected_sha256 {
            return Err(temp.die(StorageError::ChecksumMismatch));
        }
        // Make the data durable and prove that all of it is on the file: the
        // length of the handle must equal the bytes written.
        let (file, faults, written) = (
            Arc::clone(&temp.file),
            Arc::clone(&temp.faults),
            temp.written,
        );
        let durable = blocking_storage(move || {
            let mut guard = lock_file(&file);
            let Some(handle) = guard.as_mut() else {
                return Err(StorageError::Aborted);
            };
            if let Some(error) = faults.take_fsync_fault() {
                return Err(error);
            }
            handle.sync_all().map_err(map_io_error)?;
            let on_disk = handle.metadata().map_err(map_io_error)?.len();
            if on_disk != written {
                return Err(StorageError::Io(io::Error::other("short object on disk")));
            }
            *guard = None;
            Ok(())
        })
        .await;
        if let Err(error) = durable {
            return Err(temp.die(error));
        }

        let key = ObjectKey::generate();
        let publish = async {
            self.ensure_shards(&key).await?;
            let dest = self.layout.path_of(&key);
            if let Some(error) = temp.faults.take_rename_fault() {
                return Err(error);
            }
            let (from, to) = (temp.path.clone(), dest.clone());
            blocking_storage(move || link_no_overwrite(&from, &to)).await?;
            Ok(dest)
        }
        .await;
        match publish {
            Ok(dest) => {
                // Published: the object exists under its key. Remove the
                // temporary name, then make the object read-only.
                temp.finished = true;
                if let Err(e) = fs::remove_file(&temp.path).await {
                    tracing::warn!(error = %e, "cannot remove temporary name after publishing");
                }
                match fs::metadata(&dest).await {
                    Ok(meta) => {
                        let mut permissions = meta.permissions();
                        permissions.set_readonly(true);
                        if let Err(e) = fs::set_permissions(&dest, permissions).await {
                            tracing::warn!(error = %e, "cannot make the object read-only");
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "cannot read the published object"),
                }
                for dir in [
                    dest.parent(),
                    dest.parent().and_then(Path::parent),
                    Some(self.layout.objects.as_path()),
                    Some(self.layout.tmp.as_path()),
                ]
                .into_iter()
                .flatten()
                {
                    if let Err(e) = sync_dir(dir).await {
                        tracing::warn!(error = %e, "cannot fsync storage directory");
                    }
                }
                Ok(key)
            }
            Err(error) => Err(temp.die(error)),
        }
    }

    async fn abort(&self, mut temp: TempObject) {
        temp.discard();
    }

    async fn open_read(
        &self,
        key: &ObjectKey,
        range: Option<ByteRange>,
    ) -> Result<ObjectReader, StorageError> {
        let (mut file, meta) = self.open_object(key).await?;
        let len = meta.len();
        let (start, end) = match range {
            None => (0, len),
            Some(range) => {
                let end = range.end.map_or(len, |end| end.min(len));
                if range.start > len || (range.end.is_some() && end < range.start) {
                    return Err(StorageError::RangeNotSatisfiable);
                }
                (range.start, end)
            }
        };
        if start > 0 && start < len {
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(map_io_error)?;
        }
        Ok(ObjectReader {
            file,
            remaining: end.saturating_sub(start),
        })
    }

    async fn head(&self, key: &ObjectKey) -> Result<ObjectHead, StorageError> {
        let (_file, meta) = self.open_object(key).await?;
        Ok(ObjectHead { len: meta.len() })
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), StorageError> {
        let (file, _meta) = self.open_object(key).await?;
        drop(file);
        let path = self.layout.path_of(key);
        clear_readonly(&path).await;
        fs::remove_file(&path).await.map_err(map_io_error)
    }

    async fn verify(
        &self,
        key: &ObjectKey,
        expected_sha256: &Sha256Digest,
    ) -> Result<(), StorageError> {
        let mut reader = self.open_read(key, None).await?;
        let mut hasher = Sha256::new();
        while let Some(chunk) = reader.next_chunk().await? {
            hasher.update(&chunk);
        }
        let digest: Sha256Digest = hasher.finalize().into();
        if digest == *expected_sha256 {
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
        let mut found = Vec::new();
        if limit == 0 {
            return Ok(found);
        }
        let after_key = after.map(ObjectKey::as_str);
        for first in smallest_names(&self.layout.objects, 2, true, None, 256).await? {
            // Skip shards that sort entirely before the cursor.
            if after_key.is_some_and(|a| a.get(..2).is_some_and(|p| first.as_str() < p)) {
                continue;
            }
            let first_dir = self.layout.objects.join(&first);
            for second in smallest_names(&first_dir, 2, true, None, 256).await? {
                if after_key.is_some_and(|a| {
                    a.get(..4)
                        .is_some_and(|p| format!("{first}{second}").as_str() < p)
                }) {
                    continue;
                }
                let second_dir = first_dir.join(&second);
                // Entries picked by the bounded scan can turn out unusable
                // (vanished, wrong shard, not a plain file). Then the same
                // directory is read again after the last name seen, so no
                // later object is skipped.
                let mut cursor: Option<String> = after_key.map(str::to_owned);
                loop {
                    let room = limit - found.len();
                    let names =
                        smallest_names(&second_dir, 32, false, cursor.as_deref(), room).await?;
                    let exhausted = names.len() < room;
                    for name in names {
                        cursor = Some(name.clone());
                        let Ok(key) = ObjectKey::parse(&name) else {
                            continue;
                        };
                        // Entries whose shards do not match the key are not ours.
                        if key.shard1() != first || key.shard2() != second {
                            continue;
                        }
                        let meta = match fs::symlink_metadata(second_dir.join(&name)).await {
                            Ok(meta) if meta.is_file() && !is_link(&meta) => meta,
                            _ => continue,
                        };
                        found.push(ObjectInfo {
                            key,
                            len: meta.len(),
                            modified: meta.modified().ok(),
                        });
                        if found.len() >= limit {
                            return Ok(found);
                        }
                    }
                    if exhausted {
                        break;
                    }
                }
            }
        }
        Ok(found)
    }

    async fn list_temp(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<TempInfo>, StorageError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // Keep only the `limit` smallest ids after the cursor: bounded memory
        // however many temporary files exist.
        let mut heap: BinaryHeap<String> = BinaryHeap::new();
        let mut dir = fs::read_dir(&self.layout.tmp).await.map_err(map_io_error)?;
        while let Some(entry) = dir.next_entry().await.map_err(map_io_error)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".part") else {
                continue;
            };
            if !temp_id_ok(id) || after.is_some_and(|a| id <= a) {
                continue;
            }
            heap.push(id.to_owned());
            if heap.len() > limit {
                heap.pop();
            }
        }
        let mut ids = heap.into_vec();
        ids.sort_unstable();
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            match fs::symlink_metadata(self.layout.temp_path(&id)).await {
                Ok(meta) if meta.is_file() && !is_link(&meta) => out.push(TempInfo {
                    id,
                    len: meta.len(),
                    modified: meta.modified().ok(),
                }),
                _ => {}
            }
        }
        Ok(out)
    }

    async fn remove_temp(&self, id: &str) -> Result<(), StorageError> {
        if !temp_id_ok(id) {
            return Err(StorageError::NotFound);
        }
        let path = self.layout.temp_path(id);
        match fs::symlink_metadata(&path).await {
            Ok(meta) if meta.is_file() && !is_link(&meta) => {}
            Ok(_) => return Err(unsafe_tree()),
            Err(e) => return Err(map_io_error(e)),
        }
        // Never clear the read-only flag here: a temporary name can be a hard
        // link to a published object (a removal that failed after publishing),
        // and the object must stay read-only. Temporary files are not
        // read-only, and std removes read-only links where the platform needs it.
        fs::remove_file(&path).await.map_err(map_io_error)
    }
}

/// Makes a file writable so that it can be deleted on every platform.
/// Marker, quarantine: the identity of a store and a holding area for objects
/// that reconciliation took out of service.
impl LocalStore {
    /// The store identity from the marker file `craftcloud-store.json` in the
    /// root, `None` if there is no marker.
    ///
    /// # Errors
    ///
    /// [`StorageError::Io`] if the marker exists but is not a regular file or
    /// does not contain a valid identity.
    pub async fn read_marker(&self) -> Result<Option<Uuid>, StorageError> {
        Ok(self.read_marker_info().await?.map(|info| info.store_id))
    }

    /// Identity and epoch from the marker file (a marker without an epoch
    /// counts as epoch 0).
    ///
    /// # Errors
    ///
    /// As [`LocalStore::read_marker`].
    pub async fn read_marker_info(&self) -> Result<Option<MarkerInfo>, StorageError> {
        let path = self.layout.root.join(MARKER_FILE);
        match fs::symlink_metadata(&path).await {
            Ok(meta) if meta.is_file() && !is_link(&meta) => {}
            Ok(_) => return Err(unsafe_tree()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(map_io_error(e)),
        }
        let text = fs::read_to_string(&path).await.map_err(map_io_error)?;
        let invalid = || StorageError::Io(io::Error::other("invalid store marker"));
        let store_id = text
            .split("\"store_id\"")
            .nth(1)
            .and_then(|rest| rest.split('"').nth(1))
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(invalid)?;
        let epoch = match text.split("\"epoch\"").nth(1) {
            None => 0,
            Some(rest) => rest
                .trim_start()
                .strip_prefix(':')
                .map(|digits| {
                    digits
                        .trim_start()
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect::<String>()
                })
                .and_then(|digits| digits.parse::<u64>().ok())
                .ok_or_else(invalid)?,
        };
        Ok(Some(MarkerInfo { store_id, epoch }))
    }

    /// Writes the marker with epoch 0; see [`LocalStore::write_marker_info`].
    ///
    /// # Errors
    ///
    /// [`StorageError::AlreadyExists`] or an I/O error.
    pub async fn write_marker(&self, id: Uuid, replace: bool) -> Result<(), StorageError> {
        self.write_marker_info(
            MarkerInfo {
                store_id: id,
                epoch: 0,
            },
            replace,
        )
        .await
    }

    /// Writes the marker atomically (a temporary file in `tmp`, read-only) and
    /// publishes it: a hard link when it must not exist yet, a rename over the
    /// old file with `replace` (there is no moment without a marker).
    ///
    /// # Errors
    ///
    /// [`StorageError::AlreadyExists`] or an I/O error.
    pub async fn write_marker_info(
        &self,
        info: MarkerInfo,
        replace: bool,
    ) -> Result<(), StorageError> {
        let target = self.layout.root.join(MARKER_FILE);
        let temp = self
            .layout
            .tmp
            .join(format!("{}.marker", Uuid::new_v4().simple()));
        let body = format!(
            "{{\"store_id\":\"{}\",\"epoch\":{}}}\n",
            info.store_id, info.epoch
        );
        let (target2, temp2) = (target.clone(), temp.clone());
        let result = blocking(move || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp2)?;
            file.write_all(body.as_bytes())?;
            file.sync_all()?;
            drop(file);
            let mut permissions = std::fs::metadata(&temp2)?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&temp2, permissions)?;
            if replace {
                // The old marker is read-only; a replacing rename needs it writable.
                if let Ok(meta) = std::fs::metadata(&target2) {
                    let mut old = meta.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    old.set_readonly(false);
                    let _ = std::fs::set_permissions(&target2, old);
                }
                std::fs::rename(&temp2, &target2)
            } else {
                std::fs::hard_link(&temp2, &target2)
            }
        })
        .await;
        // Never clear the read-only flag here: after a hard link the temporary
        // name is the marker itself (std removes read-only links on its own).
        let _ = fs::remove_file(&temp).await;
        result.map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                StorageError::AlreadyExists
            } else {
                map_io_error(e)
            }
        })
    }

    /// Moves a published object out of `objects` into
    /// `quarantine/<day>/<key>` (same file system, one rename). The object can
    /// no longer be read as an object; an operator can move it back.
    ///
    /// # Errors
    ///
    /// [`StorageError::NotFound`] if there is no such object, or an I/O error.
    pub async fn quarantine(&self, key: &ObjectKey, day: &str) -> Result<(), StorageError> {
        if !day_ok(day) {
            return Err(StorageError::NotFound);
        }
        let (file, _meta) = self.open_object(key).await?;
        drop(file);
        let dir = self.layout.root.join(QUARANTINE_DIR).join(day);
        fs::create_dir_all(&dir).await.map_err(map_io_error)?;
        let target = dir.join(key.as_str());
        fs::rename(self.layout.path_of(key), target)
            .await
            .map_err(map_io_error)
    }

    /// Deletes the quarantine day directories named before `before_day`
    /// (`yyyymmdd`) and returns how many quarantined files that removed (or,
    /// with `dry_run`, would remove).
    ///
    /// # Errors
    ///
    /// An I/O error.
    pub async fn purge_quarantine(
        &self,
        before_day: &str,
        dry_run: bool,
    ) -> Result<u64, StorageError> {
        let root = self.layout.root.join(QUARANTINE_DIR);
        let mut entries = match fs::read_dir(&root).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(map_io_error(e)),
        };
        let mut removed = 0_u64;
        while let Some(entry) = entries.next_entry().await.map_err(map_io_error)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !day_ok(&name) || name.as_str() >= before_day {
                continue;
            }
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).await.map_err(map_io_error)?;
            if !meta.is_dir() || is_link(&meta) {
                continue;
            }
            let mut files = fs::read_dir(&path).await.map_err(map_io_error)?;
            while let Some(file) = files.next_entry().await.map_err(map_io_error)? {
                let file_path = file.path();
                if dry_run {
                    removed += 1;
                    continue;
                }
                clear_readonly(&file_path).await;
                fs::remove_file(&file_path).await.map_err(map_io_error)?;
                removed += 1;
            }
            if !dry_run {
                fs::remove_dir(&path).await.map_err(map_io_error)?;
            }
        }
        Ok(removed)
    }
}

/// `yyyymmdd`: eight ASCII digits.
fn day_ok(day: &str) -> bool {
    day.len() == 8 && day.bytes().all(|b| b.is_ascii_digit())
}

async fn clear_readonly(path: &Path) {
    if let Ok(meta) = fs::metadata(path).await {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions).await;
        }
    }
}

/// The `limit` smallest names in `dir` that are lowercase hex of `len`
/// characters and sort after `after`, in order. Shard directories (`dirs`)
/// must be plain directories, object entries files. Memory is bounded by
/// `limit` however many entries the directory holds.
async fn smallest_names(
    dir: &Path,
    len: usize,
    dirs: bool,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<String>, StorageError> {
    let mut heap: BinaryHeap<String> = BinaryHeap::new();
    let mut read = fs::read_dir(dir).await.map_err(map_io_error)?;
    while let Some(entry) = read.next_entry().await.map_err(map_io_error)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        let hex = name.len() == len
            && name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !hex || after.is_some_and(|a| name.as_str() <= a && !dirs) {
            continue;
        }
        let Ok(kind) = entry.file_type().await else {
            continue;
        };
        if kind.is_symlink() || (dirs && !kind.is_dir()) || (!dirs && !kind.is_file()) {
            continue;
        }
        heap.push(name);
        if heap.len() > limit {
            heap.pop();
        }
    }
    let mut names = heap.into_vec();
    names.sort_unstable();
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!("cc-storage-unit-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap_or_else(|e| panic!("{e}"));
        path
    }

    #[test]
    fn publishing_by_hard_link_never_overwrites() {
        let root = dir();
        let (src, other, dst) = (root.join("src"), root.join("other"), root.join("dst"));
        std::fs::write(&src, b"new").unwrap_or_else(|e| panic!("{e}"));
        std::fs::write(&other, b"existing").unwrap_or_else(|e| panic!("{e}"));
        // Free destination: linked, both names show the data.
        link_no_overwrite(&src, &dst).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::read(&dst).unwrap_or_default(), b"new");
        // Existing destination (also a read-only one): typed collision, content untouched.
        std::fs::remove_file(&dst).unwrap_or_else(|e| panic!("{e}"));
        std::fs::copy(&other, &dst).unwrap_or_else(|e| panic!("{e}"));
        let mut permissions = std::fs::metadata(&dst)
            .unwrap_or_else(|e| panic!("{e}"))
            .permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&dst, permissions).unwrap_or_else(|e| panic!("{e}"));
        assert!(matches!(
            link_no_overwrite(&src, &dst),
            Err(StorageError::AlreadyExists)
        ));
        assert_eq!(std::fs::read(&dst).unwrap_or_default(), b"existing");
        clear_readonly_sync(&dst);
        let _ = std::fs::remove_dir_all(&root);
    }
}
