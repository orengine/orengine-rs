use std::future::Future;
use std::io::Result;
use std::pin::Pin;
use std::task::{Context, Poll};
use orengine_macros::poll_for_io_request;
use crate::io::io_request::{IoRequest};
use crate::io::sys::OsPath::OsPath;
use crate::io::worker::{IoWorker, local_worker};

/// `remove` io operation which allows to remove a file from a given path.
#[must_use = "Future must be awaited to drive the IO operation"]
pub struct Remove {
    path: OsPath,
    io_request: Option<IoRequest>
}

impl Remove{
    /// Creates a new `remove` io operation.
    pub fn new(path: OsPath) -> Self {
        Self {
            path,
            io_request: None
        }
    }
}

impl Future for Remove {
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let worker = unsafe { local_worker() };
        #[allow(unused)]
        let ret;

        poll_for_io_request!((
            worker.remove_file(this.path.as_ptr(), this.io_request.as_mut().unwrap_unchecked()),
            ()
        ));
    }
}