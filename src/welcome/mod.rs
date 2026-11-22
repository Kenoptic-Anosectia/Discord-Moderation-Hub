//! Welcome message feature (`docs/verification/WELCOME-RENDER.md`).
//!
//! [`render`] is the pure, Discord-free core: it loads a JSON welcome template from a
//! `&str`, refuses an invalid template at load time, and renders it for a joining member
//! into plain data. It performs no I/O, reads no clock, and holds no serenity types.
//!
//! The public surface is [`render`] itself. The welcome runtime card WE2 calls
//! `welcome::render::WelcomeTemplate::load` and `welcome::render::render_welcome`;
//! the wiring card W3 ("Wiring C: welcome, embeds, bump, preflight print") registers the
//! feature and removes this allow.
//!
//! [`runtime`] is the thin runtime layer WE2 adds: it grants the join roles
//! through the role service and posts the rendered welcome. [`config`] parses the
//! four configuration keys of the feature.
//!
//! Unwired building block: no production caller exists yet. Wiring card W3 removes this
//! allow once the runtime calls into the module.
#![allow(dead_code)]

pub mod config;
pub mod render;
pub mod runtime;
