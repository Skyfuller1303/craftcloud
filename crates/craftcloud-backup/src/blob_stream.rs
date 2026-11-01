//! Bounded asynchronous iteration over `blobs.jsonl`.

use std::path::Path;

use tokio::sync::mpsc;

use crate::{
    error::{BackupError, ErrorKind},
    manifest::BlobEntry,
    verify::{BlobListSum, for_each_blob},
};

/// Blob entries read on a blocking thread through a bounded channel.
pub(crate) struct BlobEntryStream {
    receiver: mpsc::Receiver<Result<BlobEntry, BackupError>>,
    producer: Option<tokio::task::JoinHandle<Result<BlobListSum, BackupError>>>,
    summary: Option<BlobListSum>,
}

impl BlobEntryStream {
    /// Returns the next entry, then `None` after validating the complete list.
    pub(crate) async fn next(&mut self) -> Result<Option<BlobEntry>, BackupError> {
        match self.receiver.recv().await {
            Some(Ok(entry)) => Ok(Some(entry)),
            Some(Err(error)) => Err(error),
            None => {
                if let Some(producer) = self.producer.take() {
                    self.summary = Some(producer.await.map_err(|_| {
                        BackupError::new(ErrorKind::Io, "reading the blob list failed")
                    })??);
                }
                Ok(None)
            }
        }
    }

    /// Returns checksum/count totals after `next` returned `None`.
    pub(crate) fn summary(&self) -> Option<&BlobListSum> {
        self.summary.as_ref()
    }
}

pub(crate) fn blob_entries(dir: &Path) -> BlobEntryStream {
    let list_dir = dir.to_path_buf();
    let (sender, receiver) = mpsc::channel::<Result<BlobEntry, BackupError>>(16);
    let producer = tokio::task::spawn_blocking(move || {
        let result = for_each_blob(&list_dir, |entry| {
            sender
                .blocking_send(Ok(entry.clone()))
                .map_err(|_| BackupError::new(ErrorKind::Io, "the blob list reader was cancelled"))
        });
        if let Err(error) = &result {
            let _ = sender.blocking_send(Err(error.clone()));
        }
        result
    });
    BlobEntryStream {
        receiver,
        producer: Some(producer),
        summary: None,
    }
}
