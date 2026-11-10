//! The workspace's only FFI boundary: every `unsafe` call the tray makes into
//! AppKit, Foundation or libc, behind a safe function whose signature makes
//! the call sound -- the right kind of value for each attribute key, a
//! retained target for every action, a monitor removed exactly once.
//!
//! Every other crate is `#![forbid(unsafe_code)]`. Each block here does one
//! unsafe operation and says why it is sound in a `// SAFETY:` line;
//! `clippy::undocumented_unsafe_blocks` and
//! `clippy::multiple_unsafe_ops_per_block` hold it to that.

#![expect(unsafe_code, reason = "the workspace's only FFI boundary")]

pub mod action;
pub mod canvas;
pub mod consts;
pub mod controls;
pub mod delegate;
pub mod draw;
pub mod event;
pub mod notify;
pub mod process;
pub mod system;
pub mod text;
pub mod timer;
pub mod window;
