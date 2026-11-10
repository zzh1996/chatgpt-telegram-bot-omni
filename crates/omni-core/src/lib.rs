//! Shared domain types for the omni bot.
//!
//! - [`content`]: provider-neutral conversation model (turns, parts, reasoning,
//!   citations, native replay payloads).
//! - [`blob`]: content-addressed blob references.
//! - [`trigger`]: parser for the `alias+opt$ text` trigger syntax and
//!   `::role` synthetic-history markers.
//! - [`usage`]: token usage and cost.
//! - [`json`]: JSON helpers (merge patch, blob redaction for logs).

pub mod blob;
pub mod content;
pub mod json;
pub mod trigger;
pub mod usage;

pub use blob::BlobRef;
pub use content::{Citation, NativeOutput, Origin, Part, Reasoning, ReasoningKind, Role, ToolCall, Turn};
pub use usage::Usage;
