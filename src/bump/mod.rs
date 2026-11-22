//! Bump confirmation and reminder-cycle policy (`docs/verification/BUMP-POLICY.md`).
//!
//! [`policy`] is the pure, Discord-free decision core that replaces Bump Buddy: it decides
//! whether a message is a genuine bump confirmation from the configured listing bot and
//! whether the next reminder is due. It performs no I/O, reads no clock and holds no
//! serenity types; time and ids are passed in.
//!
//! [`config`] parses the `BUMP_*` keys through the shared settings helpers.
//! [`repository`] stores the cycle in the `bump_state` table of the shared core
//! database. [`runtime`] ties the three together: it detects a bump, persists the
//! cycle, schedules one reminder per cycle through the job queue and holds the
//! `/bump status` and staff-only `/bump reset` logic.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the policy.
#![allow(dead_code)]

pub mod config;
pub mod policy;
pub mod repository;
pub mod runtime;
