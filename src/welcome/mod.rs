//! Welcome message feature (`docs/verification/WELCOME-RENDER.md`).
//!
//! [`render`] is the pure, Discord-free core: it loads a JSON welcome template from a
//! `&str`, refuses an invalid template at load time, and renders it for a joining member
//! into plain data. It performs no I/O, reads no clock, and holds no serenity types.
//!
//! The public surface is [`render`] itself; the wiring card (W3) calls
//! `welcome::render::WelcomeTemplate::load` and `welcome::render::render_welcome`.
//!
//! Unwired building block: no production caller exists yet. Wiring card W1 removes this
//! allow once the runtime calls into the module.
#![allow(dead_code)]

pub mod render;

pub use render::{
    render_welcome, welcome_policy, IdMap, MemberContext, RenderedEmbed, RenderedField,
    RenderedWelcome, SkipReason, TemplateError, TemplateErrorKind, WelcomeTemplate,
    MAX_DESCRIPTION_CHARS, MAX_EMBED_FIELDS, MAX_EMBED_TOTAL_CHARS, MAX_FIELD_NAME_CHARS,
    MAX_FIELD_VALUE_CHARS, MAX_FOOTER_CHARS, MAX_TITLE_CHARS,
};
