//! Activity accounting feature (pure policy, Discord-free).
//!
//! [`policy`] is the pure, Discord-free accounting core behind the levels and statistics
//! features that replace the old Lurkr and Statbot bots: given plain message and voice
//! facts and a validated [`policy::ActivityConfig`], it produces the counter increments
//! and the voice-session records that the persistence card (AC2) stores.
//!
//! It performs no I/O, reads no clock and holds no serenity types: every timestamp and
//! every id is passed in by the caller.
//!
//! Privacy notes:
//! * Message CONTENT is not an input. The plain event type has no content field, so
//!   content can neither be read nor stored.
//! * The policy reads no environment variable and no file. The configuration keys are
//!   listed as constants for the wiring card to read.
//! * Anomalies are stable enum codes. They never carry message text, user names or ids.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the policy.
#![allow(dead_code)]

pub mod policy;
