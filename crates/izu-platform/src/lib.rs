#![forbid(unsafe_code)]
//! Descriptor-relative filesystem boundary. No path component is followed through a
//! symlink. The platform syscalls are provided by the general-purpose `rustix` crate.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub use unsupported::*;
