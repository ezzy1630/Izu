#![deny(unsafe_code)]
//! Descriptor-relative filesystem boundary. No path component is followed through a
//! symlink. The platform syscalls are provided by the general-purpose `rustix` crate.

mod identity;
pub use identity::FileIdentity;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub use unsupported::*;
