//! Temporary user-created voice rooms (`docs/verification/VOICE-ROOMS.md`).
//!
//! [`policy`] is the pure, Discord-free decision core of the voice-room feature that
//! replaces the old Chai bot: it decides whether a hub join creates a room, whether an
//! owner may permit a member, what a leaving owner leaves behind, and what the room name
//! and permission overwrites may look like. It performs no I/O, reads no clock and holds
//! no serenity types; `now` and every id are passed in by the caller.
//!
//! The public surface is [`policy`] itself; the wiring card (W1) calls
//! `voice_rooms::policy::decide_join`, `voice_rooms::policy::decide_permit`,
//! `voice_rooms::policy::decide_leave` and `voice_rooms::policy::decide_rename`.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the policy.
#![allow(dead_code)]

pub mod policy;
