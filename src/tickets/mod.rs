pub mod archive;
pub mod archive_index;
pub mod archive_manage;
// Unwired A08 building block; the wiring card removes this allow.
#[allow(dead_code)]
pub mod archive_remove;
pub mod archive_scan;
pub mod closer;
pub mod controller;
/// Shared A09 fixture factory, loaded once for all in-crate tests.
#[cfg(test)]
#[path = "../../tests/support/ticket_archive_fixtures.rs"]
mod fixtures;
pub mod policy;
pub mod privacy;
pub mod repository;
pub mod repository_close;
pub mod restore;
pub mod retention;
pub mod runtime;
pub mod runtime_archives;
pub mod runtime_close;
