pub mod archive;
pub mod closer;
pub mod controller;
/// Shared A09 fixture factory, loaded once for all in-crate tests.
#[cfg(test)]
#[path = "../../tests/support/ticket_archive_fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
pub mod policy;
pub mod privacy;
pub mod repository;
pub mod repository_close;
pub mod runtime;
pub mod runtime_close;
