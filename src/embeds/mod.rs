//! Managed embed posts (`docs/verification/MANAGED-EMBEDS.md`).
//!
//! This module is the Discord-free core of the "managed posts" feature: rules and
//! information embeds that are posted ONCE and then EDITED IN PLACE when the owner
//! changes a template. They are not duplicated, except for the window between a
//! successful post and the record of it, which `posts` names.
//!
//! * [`template`] loads an embed template from JSON, validates the Discord embed
//!   limits and computes a stable SHA-256 content hash of the canonical form.
//! * [`posts`] decides what to do with one key (create / edit / do nothing / refuse)
//!   and stores the record of the published message in SQLite.
//!
//! Nothing here talks to Discord: there is no serenity type, no I/O outside the
//! repository, no clock read and no randomness inside the decision logic. The planner
//! is a pure function and the repository takes `now` as a parameter.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the module.
#![allow(dead_code)]

pub mod posts;
pub mod template;
