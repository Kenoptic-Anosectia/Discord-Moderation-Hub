//! Single-target role administration, Carl style (`docs/verification/ROLE-ADMIN.md`).
//!
//! `/role add`, `/role remove`, `/role info` and `/role list` as testable logic behind small
//! ports (role facts, role executor, safeword lookup, audit sink, clock). Every grant goes
//! through the shared role service: the K03 decision, the K04 executor and the RG2 facts
//! source. Nothing here talks to Discord, reads the clock or registers a command.
//!
//! Unwired building block: no production caller exists yet. Wiring card W4 removes this
//! allow once a runtime calls into the module.
#![allow(dead_code)]

pub mod command;
