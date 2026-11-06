//! STUB: to be implemented. See src/sys/mod.rs for the Inner contract.
#![allow(unused)]
use crate::flags::EfdFlags;
use std::io;

#[derive(Debug)]
pub(crate) struct Inner;

impl Inner {
    pub(crate) fn new(_initval: u32, _flags: EfdFlags) -> io::Result<Self> {
        todo!()
    }
    pub(crate) fn read(&self) -> io::Result<u64> {
        todo!()
    }
    pub(crate) fn write(&self, _value: u64) -> io::Result<()> {
        todo!()
    }
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        todo!()
    }
    pub(crate) fn set_nonblocking(&self, _nb: bool) -> io::Result<()> {
        todo!()
    }
    pub(crate) fn is_nonblocking(&self) -> io::Result<bool> {
        todo!()
    }
}

impl std::os::windows::io::AsSocket for Inner {
    fn as_socket(&self) -> std::os::windows::io::BorrowedSocket<'_> {
        todo!()
    }
}
#[cfg(feature = "mio")]
impl Inner {
    pub(crate) fn register(
        &mut self,
        _r: &mio::Registry,
        _t: mio::Token,
        _i: mio::Interest,
    ) -> io::Result<()> {
        todo!()
    }
    pub(crate) fn reregister(
        &mut self,
        _r: &mio::Registry,
        _t: mio::Token,
        _i: mio::Interest,
    ) -> io::Result<()> {
        todo!()
    }
    pub(crate) fn deregister(&mut self, _r: &mio::Registry) -> io::Result<()> {
        todo!()
    }
}
