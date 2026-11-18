//! Flash-media expiry feature (`docs/verification/FLASH-POLICY.md`).
//!
//! `policy` is the pure, Discord-free decision core: given plain message facts and a
//! validated [`policy::FlashConfig`], it decides whether the message and the bot's notice
//! are deleted after a delay, renders the notice text, and computes the allowed-mention
//! set. It performs no I/O, reads no clock, and holds no serenity types.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the policy.
#![allow(dead_code)]

pub mod policy;

pub use policy::{
    deadline, decide, is_flash_channel, AllowedMentions, ConfigError, ConfigErrorKind, Decision,
    Deletions, ExpirePlan, FlashConfig, IgnoreReason, MessageFacts, Notice,
};
