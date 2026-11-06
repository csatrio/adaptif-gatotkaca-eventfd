//! STUB: to be implemented. See src/sys/mod.rs for the Inner contract.
#![allow(unused)]
use std::io;
use crate::flags::EfdFlags;

#[derive(Debug)]
pub(crate) struct Inner;

impl Inner {
    pub(crate) fn new(_initval: u32, _flags: EfdFlags) -> io::Result<Self> { todo!() }
    pub(crate) fn read(&self) -> io::Result<u64> { todo!() }
    pub(crate) fn write(&self, _value: u64) -> io::Result<()> { todo!() }
    pub(crate) fn try_clone(&self) -> io::Result<Self> { todo!() }
    pub(crate) fn set_nonblocking(&self, _nb: bool) -> io::Result<()> { todo!() }
    pub(crate) fn is_nonblocking(&self) -> io::Result<bool> { todo!() }
}

impl std::os::fd::AsFd for Inner { fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> { todo!() } }
