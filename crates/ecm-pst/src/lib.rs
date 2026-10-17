//! Read-only PST source.
//!
//! Built on Microsoft's `outlook-pst` crate. A PST file is always opened with a
//! read-only file handle, and its checksum is verified before and after a run.

#![forbid(unsafe_code)]
