// Basically a copy-paste from the tokio-pipe crate
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::ready;
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncWrite;

const MAX_LEN: usize = <libc::ssize_t>::MAX as _;

fn set_non_blocking(fd: RawFd) -> Result<(), std::io::Error> {
    let fd_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };

    if fd_flags == -1 {
        return Err(std::io::Error::last_os_error());
    }

    if (fd_flags & libc::O_NONBLOCK) == 0 {
        let set_result = unsafe { libc::fcntl(fd, libc::F_SETFL, fd_flags | libc::O_NONBLOCK) };

        if set_result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

pub struct FdWrite(AsyncFd<OwnedFd>);

impl TryFrom<OwnedFd> for FdWrite {
    type Error = std::io::Error;

    fn try_from(fd: OwnedFd) -> Result<Self, Self::Error> {
        set_non_blocking(fd.as_raw_fd())?;
        Ok(Self(AsyncFd::new(fd)?))
    }
}

impl FdWrite {
    fn poll_write_impl(self: Pin<&Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, std::io::Error>> {
        let fd = self.0.as_raw_fd();

        loop {
            let pinned = Pin::new(&self.0);
            let mut ready = match ready!(pinned.poll_write_ready(cx)) {
                Ok(ready) => ready,
                Err(err) => return Poll::Ready(Err(err)),
            };

            let write_result = unsafe { libc::write(fd, buf.as_ptr().cast(), std::cmp::min(buf.len(), MAX_LEN)) };

            if write_result == -1 {
                match std::io::Error::last_os_error() {
                    err if err.kind() == ErrorKind::WouldBlock => {
                        ready.clear_ready();
                    }
                    err => return Poll::Ready(Err(err)),
                }
            } else {
                match usize::try_from(write_result) {
                    Ok(written_bytes) => return Poll::Ready(Ok(written_bytes)),
                    Err(err) => return Poll::Ready(Err(std::io::Error::new(ErrorKind::Other, err))),
                }
            }
        }
    }
}

impl AsyncWrite for FdWrite {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, std::io::Error>> {
        self.as_ref().poll_write_impl(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}
