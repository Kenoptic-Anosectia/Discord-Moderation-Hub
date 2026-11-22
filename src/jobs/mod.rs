//! Durable job queue (`docs/verification/JOB-QUEUE.md`).
//!
//! [`queue`] is the synchronous, Discord-free storage-backed core: schedule,
//! dedupe, compare-and-swap claim, complete, retry with capped backoff, and
//! restart reconciliation driven by a caller-provided per-kind policy table.
//! It reads no clock and uses no randomness; `now` is always passed in.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1
//! removes the allow below once the runtime calls into the queue.
#![allow(dead_code)]

pub mod queue;
pub mod runner;
