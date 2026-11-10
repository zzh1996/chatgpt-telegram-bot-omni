//! Markdown rendering for Telegram.
//!
//! Pipeline: Markdown source -> [`ast`] (with source ranges) -> backend
//! ([`render`] for classic text + entities) -> [`split`] into messages that
//! respect Telegram limits.
//!
//! Design notes:
//! - We never use Telegram's server-side Markdown/HTML parsing; entities are
//!   computed locally.
//! - [`render::Mode::Raw`] shows the model output byte-for-byte.
//! - Underline is reserved for bot labels (see [`formatted::EntityKind::Underline`]).

pub mod ast;
pub mod formatted;
pub mod render;
pub mod rich;
pub mod split;

pub use formatted::{BreakPriority, EntityKind, Formatted, Span, TgEntity, utf16_len};
pub use render::{Mode, RenderOptions, render};
pub use split::{Limits, split};
