use std::future::Future;
use std::pin::Pin;
use std::io::Result;
use std::task::{Context, Poll};
use orengine_macros::{poll_for_io_request};

use crate::io::sys::{AsRawFd, RawFd};
use crate::io::io_request::{IoRequest};
use crate::io::worker::{IoWorker, local_worker};

/// `sync_all` io operation.
#[must_use = "Future must be awaited to drive the IO operation"]
pub struct SyncAll {
    fd: RawFd,
    io_request: Option<IoRequest>
}

impl SyncAll {
    /// Creates a new 'sync_all' io operation.
    pub fn new(fd: RawFd) -> Self {
        Self {
            fd,
            io_request: None
        }
    }
}

impl Future for SyncAll {
    type Output = Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let worker = unsafe { local_worker() };
        let ret;

        poll_for_io_request!((
             worker.sync_all(this.fd, this.io_request.as_mut().unwrap_unchecked()),
             ret
        ));
    }
}

/// The [`AsyncSyncAll`](AsyncSyncAll) trait provides a [`sync_all`](AsyncSyncAll::sync_all) method
/// to synchronize the data and metadata of a file with the underlying storage device.
///
/// For more details, see [`sync_all`](AsyncSyncAll::sync_all).
pub trait AsyncSyncAll: AsRawFd {
    /// Attempts to sync all OS-internal file content and metadata to disk.
    ///
    /// This function will attempt to ensure that all in-memory data reaches
    /// the filesystem before returning.
    ///
    /// This can be used to handle errors that would otherwise only be caught when
    /// the File is closed, as dropping a File will ignore all errors. Note, however,
    /// that [`sync_all`](AsyncSyncAll::sync_all) is generally more expensive than closing
    /// a file by dropping it, because the latter is not required to block until the data
    /// has been written to the filesystem.
    ///
    /// If synchronizing the metadata is not required,
    /// use [`sync_data`](crate::io::AsyncSyncData::sync_data) instead.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use orengine::buf::full_buffer;
    /// use orengine::fs::{File, OpenOptions};
    /// use orengine::io::{AsyncRead, AsyncWrite};
    /// use orengine::io::sync_all::AsyncSyncAll;
    ///
    /// # async fn foo() -> std::io::Result<()> {
    /// let options = OpenOptions::new().write(true);
    /// let mut file = File::open("example.txt", &options).await?;
    /// let mut buffer = b"Hello, world";
    /// file.write_all(buffer).await?;
    /// file.sync_all().await?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline(always)]
    fn sync_all(&self) -> SyncAll {
        SyncAll::new(self.as_raw_fd())
    }
}