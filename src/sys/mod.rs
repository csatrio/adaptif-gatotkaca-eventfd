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

#[cfg(all(any(target_os = "linux", target_os = "android"), not(feature = "force-shim")))]
mod linux;
#[cfg(all(any(target_os = "linux", target_os = "android"), not(feature = "force-shim")))]
pub(crate) use linux::Inner;

#[cfg(all(unix, not(all(any(target_os = "linux", target_os = "android"), not(feature = "force-shim")))))]
mod unix_shim;
#[cfg(all(unix, not(all(any(target_os = "linux", target_os = "android"), not(feature = "force-shim")))))]
pub(crate) use unix_shim::Inner;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::Inner;

#[cfg(not(any(unix, windows)))]
compile_error!("adaptif-gatotkaca-eventfd supports unix and windows targets only");
