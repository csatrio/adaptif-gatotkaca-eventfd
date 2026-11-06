//! Backend selection.
//!
//! Every backend exports `pub(crate) struct Inner` with this contract
//! (see `CONTRIBUTING`-style notes in each file):
//!
//! ```ignore
//! impl Inner {
//!     fn new(initval: u32, flags: EfdFlags) -> io::Result<Self>;
//!     fn read(&self) -> io::Result<u64>;
//!     fn write(&self, value: u64) -> io::Result<()>;
//!     fn try_clone(&self) -> io::Result<Self>;          // own pollable descriptor
//!     fn set_nonblocking(&self, nb: bool) -> io::Result<()>; // shared by clones
//!     fn is_nonblocking(&self) -> io::Result<bool>;
//! }
//! // unix:    impl AsFd for Inner
//! // native:  impl From<OwnedFd> for Inner, impl From<Inner> for OwnedFd
//! // windows: impl AsSocket for Inner
//! //          #[cfg(feature = "mio")] fn register/reregister/deregister (mio::event::Source signatures)
//! ```

#[cfg(gk_native)]
mod native;
#[cfg(gk_native)]
pub(crate) use native::{Inner, SHARE_FDS, TAG};

#[cfg(unix)]
pub(crate) mod unix_share;
#[cfg(unix)]
pub(crate) use unix_share::{InheritableExport, is_open};

#[cfg(any(windows, all(unix, not(gk_native))))]
mod shm;

/// Shared memory regions (eventfd shims, channels).
mod region;
#[allow(unused_imports)]
pub(crate) use region::{HugePages, RegionOptions};
pub(crate) use region::{SharedRegion, current_pid, process_alive};
#[cfg(all(unix, not(gk_native)))]
mod unix_shim;
#[cfg(all(unix, not(gk_native)))]
pub(crate) use unix_shim::{Core, Inner, PipeId, SHARE_FDS, TAG, pipe_id};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{InheritableExport, Inner, duplicate_from};

#[cfg(not(any(unix, windows)))]
compile_error!("adaptif-gatotkaca-eventfd supports unix and windows targets only");
