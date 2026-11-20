//! Bump confirmation and reminder-cycle policy (`docs/verification/BUMP-POLICY.md`).
//!
//! [`policy`] is the pure, Discord-free decision core that replaces Bump Buddy: it decides
//! whether a message is a genuine bump confirmation from the configured listing bot and
//! whether the next reminder is due. It performs no I/O, reads no clock and holds no
//! serenity types; time and ids are passed in.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the policy.
#![allow(dead_code)]

pub mod policy;
