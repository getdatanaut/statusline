//! Shared by the status line and the tray: payload types, formatting,
//! ANSI, paths, and the `gh`/`git` lookups.

#![forbid(unsafe_code)]

pub mod ansi;
pub mod format;
pub mod gh;
pub mod git;
pub mod num;
pub mod paths;
pub mod process;
pub mod types;
