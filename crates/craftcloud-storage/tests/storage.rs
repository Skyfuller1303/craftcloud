//! Tests of the local object store: streaming writes with checksums, atomic
//! publishing, fault injection (failure at byte N, fsync, rename, full
//! device), aborts, concurrency, ranges, listing and the safety checks of
//! the tree. They use real files below the system temp directory and need no
//! database.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Instant;

    use craftcloud_storage::{
        ByteRange, CHUNK_BYTES, FaultKind, FaultPlan, LocalStore, MarkerInfo, ObjectKey,
        ObjectStore, ReadSource, Sha256Digest, StorageError, WriteSink,
    };
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("cc-storage-test-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha(data: &[u8]) -> Sha256Digest {
        Sha256::digest(data).into()
    }

    async fn store() -> (TempRoot, LocalStore) {
        let root = TempRoot::new();
        let store = LocalStore::open(root.path()).await.unwrap();
        (root, store)
    }

    async fn store_with_faults() -> (TempRoot, LocalStore, Arc<FaultPlan>) {
        let root = TempRoot::new();
        let faults = Arc::new(FaultPlan::default());
        let store = LocalStore::open_with_faults(root.path(), faults.clone())
            .await
            .unwrap();
        (root, store, faults)
    }

    async fn put(store: &LocalStore, data: &[u8]) -> ObjectKey {
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        for part in data.chunks(10_000) {
            temp.write(part).await.unwrap();
        }
        store
            .finalize(temp, data.len() as u64, &sha(data))
            .await
            .unwrap()
    }

    async fn read_all(store: &LocalStore, key: &ObjectKey, range: Option<ByteRange>) -> Vec<u8> {
        let mut reader = store.open_read(key, range).await.unwrap();
        let mut out = Vec::new();
        while let Some(chunk) = reader.next_chunk().await.unwrap() {
            assert!(chunk.len() <= CHUNK_BYTES);
            out.extend_from_slice(&chunk);
        }
        out
    }

    fn count_files(dir: &Path) -> usize {
        let mut total = 0;
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    total += count_files(&path);
                } else {
                    total += 1;
                }
            }
        }
        total
    }

    /// Waits until a directory holds no files (drop cleanup runs on the blocking pool).
    async fn settled_empty(dir: &Path) {
        for _ in 0..200 {
            let parts = std::fs::read_dir(dir)
                .map(|d| {
                    d.flatten()
                        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
                        .count()
                })
                .unwrap_or(0);
            if parts == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} still holds files", dir.display());
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_add(seed).wrapping_mul(31))
            .collect()
    }

    // ---- happy path ---------------------------------------------------------------

    #[tokio::test]
    async fn write_finalize_read_head_verify_delete() {
        let (root, store) = store().await;
        let data = pattern(300_000, 7);
        let key = put(&store, &data).await;

        // Layout: objects/<2 hex>/<2 hex>/<32 hex>, nothing left in tmp, read-only.
        let k = key.as_str();
        let expected = root
            .path()
            .join("objects")
            .join(&k[0..2])
            .join(&k[2..4])
            .join(k);
        let meta = std::fs::metadata(&expected).unwrap();
        assert_eq!(meta.len(), 300_000);
        assert!(meta.permissions().readonly());
        assert_eq!(count_files(&root.path().join("tmp")), 0);

        assert_eq!(store.head(&key).await.unwrap().len, 300_000);
        assert_eq!(read_all(&store, &key, None).await, data);
        store.verify(&key, &sha(&data)).await.unwrap();
        assert!(matches!(
            store.verify(&key, &sha(b"other")).await,
            Err(StorageError::ChecksumMismatch)
        ));

        store.delete(&key).await.unwrap();
        assert!(matches!(
            store.head(&key).await,
            Err(StorageError::NotFound)
        ));
        assert!(matches!(
            store.delete(&key).await,
            Err(StorageError::NotFound)
        ));
        assert!(matches!(
            store.open_read(&key, None).await,
            Err(StorageError::NotFound)
        ));
    }

    #[tokio::test]
    async fn equal_contents_get_distinct_keys_and_nothing_is_deduplicated() {
        let (root, store) = store().await;
        let a = put(&store, b"same bytes").await;
        let b = put(&store, b"same bytes").await;
        assert_ne!(a, b);
        assert_eq!(count_files(&root.path().join("objects")), 2);
    }

    #[tokio::test]
    async fn empty_objects_work() {
        let (_root, store) = store().await;
        let key = put(&store, b"").await;
        assert_eq!(store.head(&key).await.unwrap().len, 0);
        assert!(read_all(&store, &key, None).await.is_empty());
        store.verify(&key, &sha(b"")).await.unwrap();
    }

    #[tokio::test]
    async fn an_unfinished_write_is_never_visible_as_an_object() {
        let (root, store) = store().await;
        let mut temp = store.begin_write(1000).await.unwrap();
        temp.write(&[1; 500]).await.unwrap();
        assert_eq!(temp.bytes_written(), 500);
        assert!(store.list_objects(None, 10).await.unwrap().is_empty());
        assert_eq!(count_files(&root.path().join("objects")), 0);
        let listed = store.list_temp(None, 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, temp.id());
        drop(temp);
        settled_empty(&root.path().join("tmp")).await;
        assert!(store.list_temp(None, 10).await.unwrap().is_empty());
    }

    // ---- verification failures ------------------------------------------------------

    #[tokio::test]
    async fn length_and_checksum_mismatches_discard_the_upload() {
        let (root, store) = store().await;
        let data = pattern(70_000, 1);
        for (len, digest, want_checksum) in [
            (data.len() as u64 + 1, sha(&data), false),
            (data.len() as u64 - 1, sha(&data), false),
            (data.len() as u64, sha(b"different"), true),
        ] {
            let mut temp = store.begin_write(u64::MAX).await.unwrap();
            temp.write(&data).await.unwrap();
            match store.finalize(temp, len, &digest).await {
                Err(StorageError::ChecksumMismatch) => assert!(want_checksum),
                Err(StorageError::LengthMismatch) => assert!(!want_checksum),
                other => panic!("unexpected {other:?}"),
            }
            assert_eq!(count_files(&root.path().join("objects")), 0);
            assert_eq!(count_files(&root.path().join("tmp")), 0);
        }
    }

    #[tokio::test]
    async fn the_byte_limit_is_enforced_and_kills_the_writer() {
        let (root, store) = store().await;
        // Exactly the limit is fine.
        let mut temp = store.begin_write(100).await.unwrap();
        temp.write(&[0; 100]).await.unwrap();
        let key = store.finalize(temp, 100, &sha(&[0; 100])).await.unwrap();
        assert_eq!(store.head(&key).await.unwrap().len, 100);
        // One byte more fails, also spread over many writes.
        let mut temp = store.begin_write(100).await.unwrap();
        temp.write(&[0; 60]).await.unwrap();
        assert!(matches!(
            temp.write(&[0; 41]).await,
            Err(StorageError::TooLarge)
        ));
        assert!(matches!(
            temp.write(&[0; 1]).await,
            Err(StorageError::Aborted)
        ));
        assert!(matches!(
            store.finalize(temp, 60, &sha(&[0; 60])).await,
            Err(StorageError::Aborted)
        ));
        assert_eq!(count_files(&root.path().join("tmp")), 0);
        assert_eq!(count_files(&root.path().join("objects")), 1);
        // A limit of zero accepts only empty data; usize overflow cannot happen.
        let mut temp = store.begin_write(0).await.unwrap();
        assert!(matches!(
            temp.write(&[1]).await,
            Err(StorageError::TooLarge)
        ));
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(&[1; 10]).await.unwrap();
    }

    // ---- fault injection ----------------------------------------------------------------

    /// Writes `data` in small parts and finalizes; returns the first error of
    /// either step. A write error at disk level surfaces from the write that
    /// flushes the 64 KiB block, or from `finalize` for the last block.
    async fn upload(store: &LocalStore, data: &[u8]) -> Result<ObjectKey, StorageError> {
        let mut temp = store.begin_write(u64::MAX).await?;
        for part in data.chunks(7_000) {
            temp.write(part).await?;
        }
        store.finalize(temp, data.len() as u64, &sha(data)).await
    }

    #[tokio::test]
    async fn a_write_failing_at_byte_n_never_publishes_anything() {
        for (kind, offset, size) in [
            (FaultKind::Io, 0_u64, 300_000_usize),
            (FaultKind::Io, 1, 300_000),
            (FaultKind::Io, 65_535, 300_000),
            (FaultKind::Io, 65_536, 300_000),
            (FaultKind::Io, 200_000, 300_000),
            (FaultKind::Full, 0, 300_000),
            (FaultKind::Full, 100_000, 300_000),
            // The failing block is the last one: all writes are buffered and
            // succeed, the error must surface at finalize and nothing is published.
            (FaultKind::Full, 500, 1_000),
            (FaultKind::Full, 999, 1_000),
            (FaultKind::Io, 70_000, 100_000),
            (FaultKind::Full, 299_999, 300_000),
        ] {
            let (root, store, faults) = store_with_faults().await;
            faults.fail_write_at(offset, kind);
            let data = pattern(size, 3);
            let error = match upload(&store, &data).await {
                Ok(key) => panic!("short object published as {key} (offset {offset}, size {size})"),
                Err(error) => error,
            };
            match kind {
                FaultKind::Full => assert!(matches!(error, StorageError::Full), "{error:?}"),
                FaultKind::Io => assert!(matches!(error, StorageError::Io(_)), "{error:?}"),
            }
            assert!(store.list_objects(None, 10).await.unwrap().is_empty());
            assert!(store.list_temp(None, 10).await.unwrap().is_empty());
            assert_eq!(count_files(root.path()), 0, "offset {offset} size {size}");
        }
    }

    #[tokio::test]
    async fn a_dead_writer_cannot_be_used_or_finalized() {
        let (root, store, faults) = store_with_faults().await;
        faults.fail_write_at(10, FaultKind::Full);
        let data = pattern(300_000, 4);
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        let mut failed = false;
        for part in data.chunks(7_000) {
            if temp.write(part).await.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(matches!(temp.write(&[1]).await, Err(StorageError::Aborted)));
        assert!(matches!(
            store.finalize(temp, 300_000, &sha(&data)).await,
            Err(StorageError::Aborted)
        ));
        assert_eq!(count_files(root.path()), 0);
    }

    #[tokio::test]
    async fn fsync_and_rename_failures_publish_nothing() {
        let (root, store, faults) = store_with_faults().await;
        let data = pattern(100_000, 5);

        faults.fail_fsync();
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(&data).await.unwrap();
        assert!(matches!(
            store.finalize(temp, data.len() as u64, &sha(&data)).await,
            Err(StorageError::Io(_))
        ));
        assert_eq!(count_files(root.path()), 0);

        faults.fail_rename();
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(&data).await.unwrap();
        assert!(matches!(
            store.finalize(temp, data.len() as u64, &sha(&data)).await,
            Err(StorageError::Io(_))
        ));
        assert!(store.list_objects(None, 10).await.unwrap().is_empty());
        assert!(store.list_temp(None, 10).await.unwrap().is_empty());
        assert_eq!(count_files(root.path()), 0);

        // Faults fire once; the store works afterwards.
        let key = put(&store, &data).await;
        assert_eq!(read_all(&store, &key, None).await, data);
    }

    #[tokio::test]
    async fn dropping_or_aborting_a_writer_removes_its_temporary_file() {
        let (root, store) = store().await;
        {
            let mut temp = store.begin_write(u64::MAX).await.unwrap();
            temp.write(&pattern(200_000, 9)).await.unwrap();
            assert_eq!(count_files(&root.path().join("tmp")), 1);
            // dropped here, as when a client disconnects mid-stream
        }
        settled_empty(&root.path().join("tmp")).await;

        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(&[1; 10]).await.unwrap();
        store.abort(temp).await;
        assert_eq!(count_files(&root.path().join("tmp")), 0);

        // Aborting a task that holds a writer (cancellation) also cleans up.
        let store2 = store.clone();
        let handle = tokio::spawn(async move {
            let mut temp = store2.begin_write(u64::MAX).await.unwrap();
            loop {
                temp.write(&[2; 1000]).await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        tokio::task::yield_now().await;
        handle.abort();
        let _ = handle.await;
        settled_empty(&root.path().join("tmp")).await;
        assert_eq!(count_files(&root.path().join("objects")), 0);
    }

    // ---- concurrency ------------------------------------------------------------------------

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_writers_get_distinct_keys_and_intact_data() {
        let (root, store) = store().await;
        let mut handles = Vec::new();
        for i in 0..16_u8 {
            let store = store.clone();
            handles.push(tokio::spawn(async move {
                let data = pattern(150_000 + usize::from(i) * 1000, i);
                let key = put(&store, &data).await;
                (key, data)
            }));
        }
        let mut keys = std::collections::BTreeSet::new();
        for handle in handles {
            let (key, data) = handle.await.unwrap();
            store.verify(&key, &sha(&data)).await.unwrap();
            assert_eq!(read_all(&store, &key, None).await, data);
            keys.insert(key);
        }
        assert_eq!(keys.len(), 16);
        assert_eq!(count_files(&root.path().join("objects")), 16);
        assert_eq!(count_files(&root.path().join("tmp")), 0);
    }

    // ---- ranges -------------------------------------------------------------------------------

    #[tokio::test]
    async fn range_reads_cover_the_edge_cases() {
        let (_root, store) = store().await;
        let data = pattern(200_000, 11);
        let key = put(&store, &data).await;
        let len = data.len() as u64;
        let range = |start, end| Some(ByteRange { start, end });

        for (start, end, want) in [
            (0, Some(10), 0..10),
            (5, Some(5), 5..5),
            (0, None, 0..200_000),
            (199_999, None, 199_999..200_000),
            (199_999, Some(200_000), 199_999..200_000),
            (100, Some(70_000), 100..70_000),
            (65_536, Some(131_072), 65_536..131_072),
            // end beyond the length is clamped
            (190_000, Some(u64::MAX), 190_000..200_000),
            (0, Some(len + 1), 0..200_000),
            // start == len: empty, not an error
            (len, None, 200_000..200_000),
            (len, Some(len), 200_000..200_000),
        ] {
            let got = read_all(&store, &key, range(start, end)).await;
            assert_eq!(got, data.get(want.clone()).unwrap(), "{start}..{end:?}");
        }
        // Not satisfiable: start beyond the end, end before start, overflow.
        for (start, end) in [
            (len + 1, None),
            (u64::MAX, None),
            (u64::MAX, Some(u64::MAX)),
            (10, Some(5)),
            (len + 5, Some(len + 10)),
        ] {
            assert!(
                matches!(
                    store.open_read(&key, range(start, end)).await,
                    Err(StorageError::RangeNotSatisfiable)
                ),
                "{start}..{end:?}"
            );
        }
        // Empty file: only the empty range is possible.
        let empty = put(&store, b"").await;
        assert!(read_all(&store, &empty, range(0, None)).await.is_empty());
        assert!(read_all(&store, &empty, range(0, Some(0))).await.is_empty());
        assert!(matches!(
            store.open_read(&empty, range(1, None)).await,
            Err(StorageError::RangeNotSatisfiable)
        ));
        // The reader reports what is left.
        let reader = store
            .open_read(&key, range(1000, Some(3000)))
            .await
            .unwrap();
        assert_eq!(reader.remaining(), 2000);
    }

    // ---- listing ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn listings_are_ordered_paginated_and_ignore_foreign_entries() {
        let (root, store) = store().await;
        let mut keys = Vec::new();
        for i in 0..25_u8 {
            keys.push(put(&store, &[i; 100]).await);
        }
        keys.sort();
        // Foreign entries are ignored.
        std::fs::write(root.path().join("objects").join("stray.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.path().join("objects").join("zz")).unwrap();
        std::fs::write(root.path().join("tmp").join("not-ours.txt"), b"x").unwrap();

        let mut seen = Vec::new();
        let mut after: Option<ObjectKey> = None;
        loop {
            let page = store.list_objects(after.as_ref(), 4).await.unwrap();
            assert!(page.len() <= 4);
            if page.is_empty() {
                break;
            }
            assert!(page.iter().all(|o| o.len == 100));
            after = page.last().map(|o| o.key.clone());
            seen.extend(page.into_iter().map(|o| o.key));
        }
        assert_eq!(seen, keys);
        assert!(store.list_objects(None, 0).await.unwrap().is_empty());

        // Temporary objects: ordered, paginated, removable by validated id only.
        let mut writers = Vec::new();
        for _ in 0..5 {
            let mut temp = store.begin_write(u64::MAX).await.unwrap();
            temp.write(&[1; 10]).await.unwrap();
            writers.push(temp);
        }
        let mut ids: Vec<String> = writers.iter().map(|t| t.id().to_owned()).collect();
        ids.sort();
        let first = store.list_temp(None, 3).await.unwrap();
        assert_eq!(
            first.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            ids[..3]
        );
        assert!(first.iter().all(|t| t.len <= 10));
        let rest = store.list_temp(Some(&ids[2]), 10).await.unwrap();
        assert_eq!(
            rest.iter().map(|t| t.id.clone()).collect::<Vec<_>>(),
            ids[3..]
        );
        // Reconciliation removes an orphan by id.
        store.remove_temp(&ids[0]).await.unwrap();
        assert!(matches!(
            store.remove_temp(&ids[0]).await,
            Err(StorageError::NotFound)
        ));
        for bad in [
            "",
            "../x",
            "..\\x",
            "not-hex",
            &"A".repeat(32),
            &"a".repeat(31),
        ] {
            assert!(matches!(
                store.remove_temp(bad).await,
                Err(StorageError::NotFound)
            ));
        }
        drop(writers);
        settled_empty(&root.path().join("tmp")).await;
        assert!(store.list_temp(None, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn listing_does_not_skip_objects_behind_unusable_entries() {
        let (root, store) = store().await;
        let mut keys = Vec::new();
        for i in 0..6_u8 {
            keys.push(put(&store, &[i; 10]).await);
        }
        keys.sort();
        // Plant entries that look like objects but sit in the wrong shard
        // directory and sort before the real ones; a bounded scan of one
        // would pick them first and must not end the listing.
        for key in &keys {
            let k = key.as_str();
            let leaf = root.path().join("objects").join(&k[0..2]).join(&k[2..4]);
            let prefix = if &k[0..4] == "0000" { "ffff" } else { "0000" };
            for n in 0..3 {
                std::fs::write(leaf.join(format!("{prefix}{n:028}")), b"x").unwrap();
            }
        }
        for page in [1_usize, 2, 4] {
            let mut seen = Vec::new();
            let mut after: Option<ObjectKey> = None;
            loop {
                let items = store.list_objects(after.as_ref(), page).await.unwrap();
                if items.is_empty() {
                    break;
                }
                after = items.last().map(|o| o.key.clone());
                seen.extend(items.into_iter().map(|o| o.key));
            }
            assert_eq!(seen, keys, "page size {page}");
        }
    }

    #[tokio::test]
    async fn removing_a_temp_name_that_links_to_a_published_object_keeps_it_read_only() {
        let (root, store) = store().await;
        let key = put(&store, b"published").await;
        let k = key.as_str();
        let object = root
            .path()
            .join("objects")
            .join(&k[0..2])
            .join(&k[2..4])
            .join(k);
        // A temporary name that is still a hard link of the object, as after
        // a removal that failed once publishing had succeeded.
        let id = Uuid::new_v4().simple().to_string();
        let leftover = root.path().join("tmp").join(format!("{id}.part"));
        std::fs::hard_link(&object, &leftover).unwrap();
        assert!(std::fs::metadata(&object).unwrap().permissions().readonly());
        store.remove_temp(&id).await.unwrap();
        assert!(!leftover.exists());
        assert!(std::fs::metadata(&object).unwrap().permissions().readonly());
        assert_eq!(read_all(&store, &key, None).await, b"published");
    }

    // ---- tampering and the safety of the tree -------------------------------------------------------

    #[tokio::test]
    async fn verify_detects_changed_data_and_truncated_files_fail_to_read() {
        let (root, store) = store().await;
        let data = pattern(100_000, 2);
        let key = put(&store, &data).await;
        let k = key.as_str();
        let path = root
            .path()
            .join("objects")
            .join(&k[0..2])
            .join(&k[2..4])
            .join(k);
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&path, permissions).unwrap();
        let mut tampered = data.clone();
        tampered[50_000] ^= 1;
        std::fs::write(&path, &tampered).unwrap();
        assert!(matches!(
            store.verify(&key, &sha(&data)).await,
            Err(StorageError::ChecksumMismatch)
        ));
        // Truncate after the length was read: the reader reports an I/O error.
        let mut reader = store.open_read(&key, None).await.unwrap();
        std::fs::write(&path, &tampered[..10]).unwrap();
        let mut failure = None;
        loop {
            match reader.next_chunk().await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        assert!(matches!(failure, Some(StorageError::Io(_))));
    }

    #[tokio::test]
    async fn open_refuses_unsuitable_roots() {
        let root = TempRoot::new();
        // Relative, missing, not a directory.
        assert!(LocalStore::open(Path::new("relative/dir")).await.is_err());
        assert!(
            LocalStore::open(&root.path().join("missing"))
                .await
                .is_err()
        );
        let file = root.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(LocalStore::open(&file).await.is_err());
        // objects/tmp that are plain files.
        let blocked = root.path().join("blocked");
        std::fs::create_dir(&blocked).unwrap();
        std::fs::write(blocked.join("objects"), b"x").unwrap();
        assert!(LocalStore::open(&blocked).await.is_err());
        // A good root works twice (existing layout is reused) and the probe leaves nothing.
        let good = root.path().join("good");
        std::fs::create_dir(&good).unwrap();
        LocalStore::open(&good).await.unwrap();
        LocalStore::open(&good).await.unwrap();
        assert_eq!(count_files(&good), 0);
    }

    #[tokio::test]
    async fn links_in_the_tree_are_refused() {
        let outer = TempRoot::new();
        let target = outer.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let link = |from: &Path, to: &Path| -> bool {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(to, from).is_ok()
            }
            #[cfg(windows)]
            {
                std::os::windows::fs::symlink_dir(to, from).is_ok()
            }
        };
        // A symlinked root.
        let root_link = outer.path().join("root-link");
        if !link(&root_link, &target) {
            eprintln!("symlinks cannot be created here (no privilege); skipped");
            return;
        }
        assert!(LocalStore::open(&root_link).await.is_err());
        // A symlinked `objects` or `tmp` below a real root.
        for name in ["objects", "tmp"] {
            let root = outer.path().join(format!("root-{name}"));
            std::fs::create_dir(&root).unwrap();
            assert!(link(&root.join(name), &target));
            assert!(LocalStore::open(&root).await.is_err(), "{name}");
        }
        // A shard directory replaced by a link after startup is not followed.
        let root = outer.path().join("root-shard");
        std::fs::create_dir(&root).unwrap();
        let store = LocalStore::open(&root).await.unwrap();
        let key = put(&store, b"data").await;
        let shard = root.join("objects").join(&key.as_str()[0..2]);
        std::fs::rename(&shard, outer.path().join("moved")).unwrap();
        assert!(link(&shard, &outer.path().join("moved")));
        assert!(store.open_read(&key, None).await.is_err());
        assert!(store.head(&key).await.is_err());
        assert!(store.delete(&key).await.is_err());
        assert_eq!(count_files(&target), 0);
    }

    #[tokio::test]
    async fn only_server_generated_keys_reach_paths() {
        // The key type is the only way into a path; hostile strings do not parse.
        for hostile in [
            "../../../etc/passwd",
            "..\\..\\windows\\system32",
            "objects/ab/cd/abcdef",
            "C:\\x",
            "ab\0cd",
            "",
        ] {
            assert!(ObjectKey::parse(hostile).is_err(), "{hostile:?}");
        }
        let (_root, store) = store().await;
        let missing = ObjectKey::generate();
        assert!(matches!(
            store.head(&missing).await,
            Err(StorageError::NotFound)
        ));
    }

    // ---- large ----------------------------------------------------------------------------------------

    /// Streams 1 GiB through write, finalize, read and verify with the fixed
    /// 64 KiB buffers. Run manually:
    /// `cargo test -p craftcloud-storage --test storage -- --ignored --nocapture large`
    #[tokio::test]
    #[ignore = "large: writes 1 GiB to the temp directory; run manually"]
    async fn large_one_gibibyte_streams_with_bounded_buffers() {
        const TOTAL: u64 = 1 << 30;
        const STEP: usize = 1 << 20;
        let (_root, store) = store().await;
        let block = pattern(STEP, 42);
        let started = Instant::now();
        let mut temp = store.begin_write(TOTAL).await.unwrap();
        let mut hasher = Sha256::new();
        for _ in 0..(TOTAL / STEP as u64) {
            hasher.update(&block);
            temp.write(&block).await.unwrap();
        }
        assert_eq!(temp.bytes_written(), TOTAL);
        let digest: Sha256Digest = hasher.finalize().into();
        let key = store.finalize(temp, TOTAL, &digest).await.unwrap();
        let written = started.elapsed();

        let mut reader = store.open_read(&key, None).await.unwrap();
        let mut hasher = Sha256::new();
        let (mut total, mut largest) = (0_u64, 0_usize);
        while let Some(chunk) = reader.next_chunk().await.unwrap() {
            largest = largest.max(chunk.len());
            total += chunk.len() as u64;
            hasher.update(&chunk);
        }
        let read_digest: Sha256Digest = hasher.finalize().into();
        assert_eq!(total, TOTAL);
        assert_eq!(read_digest, digest);
        // Memory is bounded by the chunk size, not by the object size.
        assert!(largest <= CHUNK_BYTES, "largest chunk {largest}");
        store.verify(&key, &digest).await.unwrap();
        store.delete(&key).await.unwrap();
        eprintln!(
            "large test: 1 GiB written+finalized in {written:?}, total with read+verify {:?}, largest chunk {largest} bytes",
            started.elapsed()
        );
    }

    // ---- marker and quarantine ---------------------------------------------------------------

    #[tokio::test]
    async fn the_marker_is_written_once_atomically_and_read_only() {
        let root = TempRoot::new();
        let store = LocalStore::open(root.path()).await.unwrap();
        assert_eq!(store.read_marker().await.unwrap(), None);
        let id = Uuid::new_v4();
        store.write_marker(id, false).await.unwrap();
        assert_eq!(store.read_marker().await.unwrap(), Some(id));
        let path = root.path().join("craftcloud-store.json");
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        // Never overwritten without `replace`.
        assert!(matches!(
            store.write_marker(Uuid::new_v4(), false).await,
            Err(StorageError::AlreadyExists)
        ));
        assert_eq!(store.read_marker().await.unwrap(), Some(id));
        let other = Uuid::new_v4();
        store.write_marker(other, true).await.unwrap();
        assert_eq!(store.read_marker().await.unwrap(), Some(other));
        // No leftovers in tmp.
        assert!(store.list_temp(None, 10).await.unwrap().is_empty());
        // A damaged marker is an error, not "no marker".
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        std::fs::write(&path, b"garbage").unwrap();
        assert!(store.read_marker().await.is_err());
    }

    #[tokio::test]
    async fn quarantine_moves_objects_out_of_service_and_purges_old_days() {
        let root = TempRoot::new();
        let store = LocalStore::open(root.path()).await.unwrap();
        let (a, b) = (put(&store, b"alpha").await, put(&store, b"beta").await);
        store.quarantine(&a, "20260101").await.unwrap();
        store.quarantine(&b, "20260201").await.unwrap();
        // Not an object any more: no read, no listing.
        assert!(matches!(
            store.open_read(&a, None).await,
            Err(StorageError::NotFound)
        ));
        assert!(store.list_objects(None, 10).await.unwrap().is_empty());
        let kept = root
            .path()
            .join("quarantine")
            .join("20260101")
            .join(a.as_str());
        assert_eq!(std::fs::read(&kept).unwrap(), b"alpha");
        // Missing objects and bad day names.
        assert!(matches!(
            store.quarantine(&a, "20260101").await,
            Err(StorageError::NotFound)
        ));
        let c = put(&store, b"gamma").await;
        assert!(store.quarantine(&c, "../x").await.is_err());
        assert!(store.quarantine(&c, "2026").await.is_err());
        assert_eq!(store.head(&c).await.unwrap().len, 5);

        // Purge: only day directories strictly before the given day.
        assert_eq!(store.purge_quarantine("20260101", false).await.unwrap(), 0);
        assert_eq!(store.purge_quarantine("20260201", true).await.unwrap(), 1);
        assert!(kept.exists());
        assert_eq!(store.purge_quarantine("20260201", false).await.unwrap(), 1);
        assert!(!kept.exists());
        assert!(
            root.path()
                .join("quarantine")
                .join("20260201")
                .join(b.as_str())
                .exists()
        );
        // Nothing to purge without a quarantine directory.
        let empty = TempRoot::new();
        let other = LocalStore::open(empty.path()).await.unwrap();
        assert_eq!(other.purge_quarantine("29991231", false).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn the_marker_epoch_round_trips_and_replacing_keeps_it_read_only() {
        let root = TempRoot::new();
        let store = LocalStore::open(root.path()).await.unwrap();
        let id = Uuid::new_v4();
        store
            .write_marker_info(
                MarkerInfo {
                    store_id: id,
                    epoch: 0,
                },
                false,
            )
            .await
            .unwrap();
        for epoch in [1_u64, 2, 40] {
            store
                .write_marker_info(
                    MarkerInfo {
                        store_id: id,
                        epoch,
                    },
                    true,
                )
                .await
                .unwrap();
            let info = store.read_marker_info().await.unwrap().unwrap();
            assert_eq!(
                info,
                MarkerInfo {
                    store_id: id,
                    epoch
                }
            );
        }
        let path = root.path().join("craftcloud-store.json");
        assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
        // A marker from before epochs counts as epoch 0.
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(false);
            std::fs::set_permissions(&path, permissions).unwrap();
        }
        std::fs::write(&path, format!("{{\"store_id\":\"{id}\"}}\n")).unwrap();
        assert_eq!(
            store.read_marker_info().await.unwrap(),
            Some(MarkerInfo {
                store_id: id,
                epoch: 0
            })
        );
        // A broken epoch is an error, not zero.
        std::fs::write(&path, format!("{{\"store_id\":\"{id}\",\"epoch\":x}}\n")).unwrap();
        assert!(store.read_marker_info().await.is_err());
    }
    #[tokio::test]
    async fn the_probe_leaves_nothing_behind_even_when_the_caller_gives_up() {
        let (root, store) = store().await;
        let tmp_files = || std::fs::read_dir(root.path().join("tmp")).unwrap().count();
        store.probe().await.unwrap();
        assert_eq!(tmp_files(), 0);
        // Many callers that are cancelled right away: the blocking jobs still
        // finish and remove their files.
        for _ in 0..50 {
            let probe = store.probe();
            let _ = tokio::time::timeout(std::time::Duration::ZERO, probe).await;
        }
        for _ in 0..200 {
            if tmp_files() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(tmp_files(), 0, "probe files remain");
    }

    #[tokio::test]
    async fn stale_probe_files_are_swept_and_other_files_are_not() {
        let (root, store) = store().await;
        let tmp = root.path().join("tmp");
        let probe = tmp.join(format!("{}.health", Uuid::new_v4().simple()));
        let other = tmp.join(format!("{}.part", Uuid::new_v4().simple()));
        std::fs::write(&probe, b"health").unwrap();
        std::fs::write(&other, b"data").unwrap();
        // Young probes stay; nothing but probes is touched.
        let young = store
            .sweep_health_probes(std::time::Duration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(young, 0);
        assert!(probe.exists());
        let all = store
            .sweep_health_probes(std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(all, 1);
        assert!(!probe.exists());
        assert!(other.exists());
    }

    // ---- publishing under a chosen key (restore of a backup) --------------------------

    #[tokio::test]
    async fn finalize_as_publishes_under_the_given_key_and_never_overwrites() {
        let (root, store) = store().await;
        let data = pattern(70_000, 3);
        let key = ObjectKey::generate();

        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(&data).await.unwrap();
        let published = store
            .finalize_as(temp, key.clone(), data.len() as u64, &sha(&data))
            .await
            .unwrap();
        assert_eq!(published, key);
        assert_eq!(read_all(&store, &key, None).await, data);

        // The same key again: refused, the first object is untouched, nothing is left over.
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(b"other").await.unwrap();
        assert!(matches!(
            store
                .finalize_as(temp, key.clone(), 5, &sha(b"other"))
                .await,
            Err(StorageError::AlreadyExists)
        ));
        assert_eq!(read_all(&store, &key, None).await, data);
        assert_eq!(count_files(&root.path().join("tmp")), 0);

        // A wrong checksum publishes nothing under the requested key.
        let other_key = ObjectKey::generate();
        let mut temp = store.begin_write(u64::MAX).await.unwrap();
        temp.write(b"bytes").await.unwrap();
        assert!(matches!(
            store
                .finalize_as(temp, other_key.clone(), 5, &sha(b"different"))
                .await,
            Err(StorageError::ChecksumMismatch)
        ));
        assert!(matches!(
            store.head(&other_key).await,
            Err(StorageError::NotFound)
        ));
    }
}
