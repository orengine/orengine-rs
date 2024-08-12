use std::future::Future;
use std::io::Result;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use io_macros::{poll_for_io_request, poll_for_time_bounded_io_request};
use socket2::SockAddr;

use crate::io::io_request::IoRequest;
use crate::io::io_sleeping_task::TimeBoundedIoTask;
use crate::io::sys::{AsRawFd, RawFd, MessageRecvHeader};
use crate::io::worker::{local_worker, IoWorker};
use crate::messages::BUG;

#[must_use = "Future must be awaited to drive the IO operation"]
pub struct RecvFrom<'fut> {
    fd: RawFd,
    msg_header: MessageRecvHeader<'fut>,
    addr: &'fut mut SockAddr,
    io_request: Option<IoRequest>,
}

impl<'fut> RecvFrom<'fut> {
    pub fn new(fd: RawFd, buf: &'fut mut [u8], addr: &'fut mut SockAddr) -> Self {
        Self {
            fd,
            msg_header: MessageRecvHeader::new(buf),
            addr,
            io_request: None,
        }
    }
}

impl<'fut> Future for RecvFrom<'fut> {
    type Output = Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let worker = unsafe { local_worker() };
        let ret;

        poll_for_io_request!((
            worker.recv_from(
                this.fd,
                this.msg_header.get_os_message_header_ptr(this.addr),
                this.io_request.as_mut().unwrap_unchecked()
            ),
            ret
        ));
    }
}

#[must_use = "Future must be awaited to drive the IO operation"]
pub struct RecvFromWithDeadline<'fut> {
    fd: RawFd,
    msg_header: MessageRecvHeader<'fut>,
    addr: &'fut mut SockAddr,
    time_bounded_io_task: TimeBoundedIoTask,
    io_request: Option<IoRequest>,
}

impl<'fut> RecvFromWithDeadline<'fut> {
    pub fn new(fd: RawFd, buf: &'fut mut [u8], addr: &'fut mut SockAddr, deadline: Instant) -> Self {
        Self {
            fd,
            msg_header: MessageRecvHeader::new(buf),
            addr,
            time_bounded_io_task: TimeBoundedIoTask::new(deadline, 0),
            io_request: None,
        }
    }
}

impl<'fut> Future for RecvFromWithDeadline<'fut> {
    type Output = Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        let worker = unsafe { local_worker() };
        let ret;

        poll_for_time_bounded_io_request!((
            worker.recv_from(
                this.fd,
                this.msg_header.get_os_message_header_ptr(this.addr),
                this.io_request.as_mut().unwrap_unchecked()
            ),
            ret
        ));
    }
}

macro_rules! generate_async_recv_from {
    ($trait_name:ident, $addr_type:ty, $cast_fn: expr) => {
        pub trait $trait_name: AsRawFd {
            #[inline(always)]
            async fn recv_from(
                &mut self,
                buf: &mut [u8]
            ) -> Result<(usize, $addr_type)> {
                let mut sock_addr = unsafe { std::mem::zeroed() };
                let n = RecvFrom::new(self.as_raw_fd(), buf, &mut sock_addr).await?;

                Ok((n, $cast_fn(&sock_addr).expect(BUG)))
            }

            #[inline(always)]
            async fn recv_from_with_deadline(
                &mut self,
                buf: &mut [u8],
                deadline: Instant
            ) -> Result<(usize, $addr_type)> {
                let mut sock_addr = unsafe { std::mem::zeroed() };
                let n = RecvFromWithDeadline::new(self.as_raw_fd(), buf, &mut sock_addr, deadline).await?;

                Ok((n, $cast_fn(&sock_addr).expect(BUG)))
            }

            #[inline(always)]
            async fn recv_from_with_timeout(
                &mut self,
                buf: &mut [u8],
                timeout: Duration
            ) -> Result<(usize, $addr_type)> {
                self.recv_from_with_deadline(buf, Instant::now() + timeout).await
            }
        }
    };
}

generate_async_recv_from!(AsyncRecvFrom, SocketAddr, SockAddr::as_socket);
generate_async_recv_from!(AsyncRecvFromUnix, std::os::unix::net::SocketAddr, SockAddr::as_unix);