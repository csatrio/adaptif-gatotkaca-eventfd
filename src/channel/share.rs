//! Moving channel ends between processes: as descriptors over a unix socket
//! (`SCM_RIGHTS`), or on Windows as an offer the receiving process pulls
//! into itself. Both rings live in one shared memory object, so an end is
//! that object plus the two doorbell eventfds.

use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::lane::{DATA, Layout};
use super::{End, GkReceiver, GkSender, Ring, Side};
use crate::GkEventFd;
use crate::sys::{SharedRegion, current_pid};

impl End {
    /// Marks this end as on its way to another process, so dropping our copy
    /// doesn't close the channel. Returns the previous owner for `restore`.
    fn hand_off(&self) -> u32 {
        self.owner().swap(0, Ordering::AcqRel)
    }

    fn restore(&self, owner: u32) {
        self.owner().store(owner, Ordering::Release);
    }

    /// Builds an end received by this process, which becomes its owner.
    fn received(ring: Ring, data_ready: GkEventFd, space_ready: GkEventFd, side: Side) -> Self {
        let end = Self::new(Arc::new(ring), data_ready, space_ready, side);
        end.owner().store(current_pid(), Ordering::Release);
        end
    }

    fn tag(side: Side) -> &'static str {
        #[cfg(unix)]
        let native = crate::sys::TAG.ends_with('n');
        #[cfg(windows)]
        let native = false;
        match (side, native) {
            (Side::Sender, true) => "gkch1:Sn",
            (Side::Sender, false) => "gkch1:Su",
            (Side::Receiver, true) => "gkch1:Rn",
            (Side::Receiver, false) => "gkch1:Ru",
        }
    }
}

#[cfg(unix)]
impl End {
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        let mut fds = vec![self.ring.region.fd()];
        fds.extend(self.data_ready.inner.share_fds());
        fds.extend(self.space_ready.inner.share_fds());
        let owner = self.hand_off();
        let r = crate::sys::unix_share::send(sock, Self::tag(self.side), &fds);
        if r.is_err() {
            self.restore(owner);
        }
        r
    }

    fn recv_over(sock: std::os::fd::BorrowedFd<'_>, side: Side) -> io::Result<Self> {
        use crate::sys::{Inner, SHARE_FDS};
        let mut fds = crate::sys::unix_share::recv(sock, Self::tag(side), 1 + 2 * SHARE_FDS)?;
        let space = fds.split_off(1 + SHARE_FDS);
        let data = fds.split_off(1);
        let region_fd = fds.pop().expect("one region descriptor");
        let region = SharedRegion::from_owned(region_fd, DATA, Layout::accepts)?;
        let data_ready = GkEventFd {
            inner: Inner::from_share_fds(data)?,
        };
        let space_ready = GkEventFd {
            inner: Inner::from_share_fds(space)?,
        };
        Ok(Self::received(
            Ring::with_region(region),
            data_ready,
            space_ready,
            side,
        ))
    }
}

#[cfg(windows)]
impl End {
    /// An offer the process `pid` can take with `take` (see
    /// `GkProcess::send`): `gkch1:<S|R>u|<region>|<eventfd offer>|<eventfd offer>`.
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        use std::os::windows::io::AsRawHandle;
        let owner = self.hand_off();
        let r = (|| {
            let keep = self.ring.region.section().try_clone_to_owned()?;
            let (d, kd) = self.data_ready.inner.offer_to(pid)?;
            let (s, ks) = self.space_ready.inner.offer_to(pid)?;
            let token = format!(
                "{}|{:x}|{d}|{s}",
                Self::tag(self.side),
                keep.as_raw_handle() as usize
            );
            Ok((token, vec![keep, kd, ks]))
        })();
        if r.is_err() {
            self.restore(owner);
        }
        r
    }

    fn take(token: &str, pid: u32, side: Side) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidInput, "invalid channel offer");
        let mut parts = token.trim().split('|');
        if parts.next() != Some(Self::tag(side)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "not the expected kind of object",
            ));
        }
        let section =
            usize::from_str_radix(parts.next().ok_or_else(bad)?, 16).map_err(|_| bad())?;
        let d = parts.next().ok_or_else(bad)?;
        let s = parts.next().ok_or_else(bad)?;
        if parts.next().is_some() {
            return Err(bad());
        }
        let region = crate::sys::duplicate_from(pid, section)?;
        let region = SharedRegion::from_owned(region, DATA, Layout::accepts)?;
        let data_ready = GkEventFd {
            inner: crate::sys::Inner::take_offer(d, pid)?,
        };
        let space_ready = GkEventFd {
            inner: crate::sys::Inner::take_offer(s, pid)?,
        };
        Ok(Self::received(
            Ring::with_region(region),
            data_ready,
            space_ready,
            side,
        ))
    }
}

impl GkSender {
    /// Unix: hands this end to the process at the other end of `stream`
    /// (`SCM_RIGHTS`), which calls [`GkSender::recv_from`]. This process's
    /// copy is detached: dropping it does not close the channel.
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        self.end.send_over(std::os::fd::AsFd::as_fd(stream))
    }

    /// Unix: receives a sender passed with [`GkSender::send_to`]; this process
    /// becomes its owner.
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self::from_end(End::recv_over(
            std::os::fd::AsFd::as_fd(stream),
            Side::Sender,
        )?))
    }
}

impl GkReceiver {
    /// Unix: hands this end to the process at the other end of `stream`
    /// (`SCM_RIGHTS`), which calls [`GkReceiver::recv_from`]. This process's
    /// copy is detached: dropping it does not close the channel.
    #[cfg(unix)]
    pub fn send_to(&self, stream: &std::os::unix::net::UnixStream) -> io::Result<()> {
        self.end.send_over(std::os::fd::AsFd::as_fd(stream))
    }

    /// Unix: receives a receiver passed with [`GkReceiver::send_to`]; this
    /// process becomes its owner.
    #[cfg(unix)]
    pub fn recv_from(stream: &std::os::unix::net::UnixStream) -> io::Result<Self> {
        Ok(Self::from_end(End::recv_over(
            std::os::fd::AsFd::as_fd(stream),
            Side::Receiver,
        )?))
    }
}

/// Lets `GkProcess` move channel ends (see `process.rs`).
impl crate::process::private::Sealed for GkSender {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.end.send_over(sock)
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        Ok(Self::from_end(End::recv_over(sock, Side::Sender)?))
    }
    #[cfg(windows)]
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        self.end.offer(pid)
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        Ok(Self::from_end(End::take(token, pid, Side::Sender)?))
    }
}

impl crate::process::private::Sealed for GkReceiver {
    #[cfg(unix)]
    fn send_over(&self, sock: std::os::fd::BorrowedFd<'_>) -> io::Result<()> {
        self.end.send_over(sock)
    }
    #[cfg(unix)]
    fn recv_over(sock: std::os::fd::BorrowedFd<'_>) -> io::Result<Self> {
        Ok(Self::from_end(End::recv_over(sock, Side::Receiver)?))
    }
    #[cfg(windows)]
    fn offer(&self, pid: u32) -> io::Result<(String, Vec<std::os::windows::io::OwnedHandle>)> {
        self.end.offer(pid)
    }
    #[cfg(windows)]
    fn take(token: &str, pid: u32) -> io::Result<Self> {
        Ok(Self::from_end(End::take(token, pid, Side::Receiver)?))
    }
}

impl crate::process::Shareable for GkSender {}
impl crate::process::Shareable for GkReceiver {}
