//! Human-readable summary of what a prepared request actually asks for — shown
//! under each reply so users can check that their options were understood.
//! The request plan produces it (the reasoning setting always, then every
//! tool and option), so it describes what is really sent.

use crate::Prepared;

/// Short descriptions, e.g. `["effort max", "web search", "image size 1K"]`.
pub fn describe(p: &Prepared) -> Vec<String> {
    p.settings.clone()
}
