//! Limit-aware splitting of [`Formatted`] text into Telegram-sized messages.
//!
//! Guarantees (property-tested):
//! - every chunk has at most `max_utf16` UTF-16 code units and at most
//!   `max_entities` entities;
//! - concatenating the chunks gives back the original text modulo whitespace
//!   removed at chunk boundaries;
//! - entity ranges are clipped, never dropped, except when a single position is
//!   covered by more than `max_entities` nested entities (impossible for our
//!   renderer, handled defensively).

use crate::formatted::{BreakPriority, Formatted, Span, Utf16Map};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_utf16: usize,
    pub max_entities: usize,
}

impl Limits {
    /// Classic Telegram text message limits.
    pub const TELEGRAM_TEXT: Limits = Limits { max_utf16: 4096, max_entities: 100 };
}

/// Minimum fill ratio before accepting a break of the given priority.
fn min_fill(prio: BreakPriority) -> f64 {
    match prio {
        BreakPriority::Section => 0.25,
        BreakPriority::Block => 0.4,
        BreakPriority::Line => 0.4,
        BreakPriority::Space => 0.7,
        BreakPriority::Char => 0.0,
    }
}

pub fn split(input: &Formatted, limits: Limits) -> Vec<Formatted> {
    assert!(limits.max_utf16 >= 16, "max_utf16 too small");
    assert!(limits.max_entities >= 1, "max_entities too small");
    let mut f = input.clone();
    f.normalize();
    let text = f.text.as_str();
    let map = Utf16Map::new(text);

    // Collect break candidates: explicit + derived from whitespace.
    let mut breaks: Vec<(usize, BreakPriority)> = f.breaks.clone();
    for (i, c) in text.char_indices() {
        let next = i + c.len_utf8();
        match c {
            '\n' => breaks.push((next, BreakPriority::Line)),
            ' ' | '\t' => breaks.push((next, BreakPriority::Space)),
            _ => {}
        }
    }
    breaks.sort_by_key(|b| b.0);
    breaks.dedup_by(|a, b| {
        if a.0 == b.0 {
            b.1 = b.1.max(a.1);
            true
        } else {
            false
        }
    });

    let mut spans_sorted: Vec<&Span> = f.spans.iter().collect();
    spans_sorted.sort_by_key(|s| s.start);

    let mut chunks = Vec::new();
    let mut start = skip_ws(text, 0);
    while start < text.len() {
        // Largest end such that the chunk fits in max_utf16.
        let mut hard_end = max_end_for_len(text, &map, start, limits.max_utf16);
        // Entity limit: the chunk must end before the (max+1)-th overlapping span starts.
        if let Some(limit) = entity_limit_end(&spans_sorted, start, hard_end, limits.max_entities) {
            hard_end = hard_end.min(limit);
        }
        if hard_end <= start {
            // Pathological (more than max nested spans at `start`): force progress by one char.
            hard_end = next_char(text, start);
        }
        let end = if hard_end >= text.len() {
            text.len()
        } else {
            choose_break(text, &map, &breaks, start, hard_end, limits.max_utf16)
        };
        let mut chunk = f.slice(start, end).trimmed();
        enforce_entity_cap(&mut chunk, limits.max_entities);
        if !chunk.text.is_empty() {
            chunks.push(chunk);
        }
        start = skip_ws(text, end);
    }
    chunks
}

fn skip_ws(text: &str, mut pos: usize) -> usize {
    while pos < text.len() {
        let c = text[pos..].chars().next().unwrap();
        if !c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

fn next_char(text: &str, pos: usize) -> usize {
    pos + text[pos..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn max_end_for_len(text: &str, map: &Utf16Map, start: usize, max_utf16: usize) -> usize {
    let base = map.at(start);
    if map.at(text.len()) - base <= max_utf16 {
        return text.len();
    }
    // Binary search over char boundaries.
    let (mut lo, mut hi) = (start, text.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let mut m = mid;
        while m > lo && !text.is_char_boundary(m) {
            m -= 1;
        }
        if m == lo {
            // no boundary between lo and mid other than lo; try advancing
            let n = next_char(text, lo);
            if n <= hi && map.at(n) - base <= max_utf16 {
                lo = n;
                continue;
            }
            break;
        }
        if map.at(m) - base <= max_utf16 {
            lo = m;
        } else {
            hi = m - 1;
            while hi > lo && !text.is_char_boundary(hi) {
                hi -= 1;
            }
        }
    }
    lo
}

/// If more than `max` spans overlap `[start, end)`, return the position before
/// which the chunk must end so that at most `max` spans overlap it.
fn entity_limit_end(spans: &[&Span], start: usize, end: usize, max: usize) -> Option<usize> {
    let overlapping: Vec<&Span> = spans.iter().copied().filter(|s| s.start < end && s.end > start).collect();
    if overlapping.len() <= max {
        return None;
    }
    // Spans are sorted by start; those starting before `start` are carried over.
    let lim = overlapping[max].start;
    Some(lim.max(start))
}

fn choose_break(
    text: &str,
    map: &Utf16Map,
    breaks: &[(usize, BreakPriority)],
    start: usize,
    hard_end: usize,
    max_utf16: usize,
) -> usize {
    let base = map.at(start);
    let candidates: Vec<&(usize, BreakPriority)> =
        breaks.iter().filter(|(p, _)| *p > start && *p <= hard_end).collect();
    for prio in [BreakPriority::Section, BreakPriority::Block, BreakPriority::Line, BreakPriority::Space] {
        let need = (max_utf16 as f64 * min_fill(prio)) as usize;
        if let Some(&&(p, _)) = candidates
            .iter()
            .rev()
            .find(|(p, pr)| *pr >= prio && text.is_char_boundary(*p) && map.at(*p) - base >= need)
        {
            return p;
        }
    }
    // Hard cut at a char boundary.
    let mut e = hard_end;
    while e > start && !text.is_char_boundary(e) {
        e -= 1;
    }
    if e == start { next_char(text, start) } else { e }
}

/// Defensive: if a chunk still exceeds the entity cap, drop innermost spans.
fn enforce_entity_cap(chunk: &mut Formatted, max: usize) {
    if chunk.spans.len() <= max {
        return;
    }
    // Keep the longest spans (outer structure), drop the shortest.
    let mut idx: Vec<usize> = (0..chunk.spans.len()).collect();
    idx.sort_by_key(|&i| std::cmp::Reverse(chunk.spans[i].end - chunk.spans[i].start));
    let keep: std::collections::HashSet<usize> = idx.into_iter().take(max).collect();
    let mut i = 0;
    chunk.spans.retain(|_| {
        let k = keep.contains(&i);
        i += 1;
        k
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::formatted::{EntityKind, utf16_len};
    use proptest::prelude::*;

    fn lim(n: usize, e: usize) -> Limits {
        Limits { max_utf16: n, max_entities: e }
    }

    #[test]
    fn short_text_single_chunk() {
        let f = Formatted::plain("hello world");
        let c = split(&f, Limits::TELEGRAM_TEXT);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "hello world");
    }

    #[test]
    fn splits_at_block_breaks() {
        let mut f = Formatted::new();
        for i in 0..10 {
            f.push_str(&format!("paragraph {i} {}", "x".repeat(30)));
            f.mark_break(BreakPriority::Block);
            f.push_str("\n\n");
        }
        let c = split(&f, lim(100, 100));
        for ch in &c {
            assert!(utf16_len(&ch.text) <= 100);
            assert!(ch.text.starts_with("paragraph"), "{:?}", ch.text);
        }
    }

    #[test]
    fn pre_is_clipped_into_each_chunk() {
        let mut f = Formatted::new();
        let code: String = (0..50).map(|i| format!("line {i}\n")).collect();
        f.push_styled(&code, EntityKind::Pre { language: "rust".into() });
        let c = split(&f, lim(64, 100));
        assert!(c.len() > 1);
        for ch in &c {
            assert_eq!(ch.spans.len(), 1);
            assert!(matches!(ch.spans[0].kind, EntityKind::Pre { .. }));
            assert!(utf16_len(&ch.text) <= 64);
        }
    }

    #[test]
    fn entity_limit_respected() {
        let mut f = Formatted::new();
        for i in 0..300 {
            f.push_styled(&format!("w{i}"), EntityKind::Bold);
            f.push_str(" ");
        }
        let c = split(&f, lim(4096, 100));
        assert!(c.len() >= 3);
        for ch in &c {
            assert!(ch.spans.len() <= 100);
        }
    }

    #[test]
    fn no_surrogate_split() {
        let s = "😀".repeat(100);
        let f = Formatted::plain(s);
        let c = split(&f, lim(17, 10)); // odd limit forces boundary handling
        for ch in &c {
            assert!(utf16_len(&ch.text) <= 17);
        }
        let joined: String = c.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(joined, "😀".repeat(100));
    }

    fn arb_formatted() -> impl Strategy<Value = Formatted> {
        let piece = prop_oneof![
            "[a-z ]{0,20}".prop_map(|s| (s, None)),
            "[a-z\\n]{1,10}".prop_map(|s| (s, None)),
            "[😀é中 ]{1,6}".prop_map(|s| (s, None)),
            "[a-z]{1,8}".prop_map(|s| (s, Some(EntityKind::Bold))),
            "[a-z ]{1,8}".prop_map(|s| (s, Some(EntityKind::Code))),
            "[a-z\\n]{1,30}".prop_map(|s| (s, Some(EntityKind::Pre { language: String::new() }))),
        ];
        proptest::collection::vec(piece, 0..60).prop_map(|pieces| {
            let mut f = Formatted::new();
            for (s, k) in pieces {
                match k {
                    Some(k) => f.push_styled(&s, k),
                    None => f.push_str(&s),
                }
            }
            f
        })
    }

    proptest! {
        #[test]
        fn chunks_respect_limits(f in arb_formatted(), max in 16usize..200, ents in 1usize..8) {
            let chunks = split(&f, lim(max, ents));
            let mut joined_nows = String::new();
            for ch in &chunks {
                prop_assert!(utf16_len(&ch.text) <= max);
                prop_assert!(ch.spans.len() <= ents);
                for s in &ch.spans {
                    prop_assert!(s.start < s.end && s.end <= ch.text.len());
                }
                joined_nows.extend(ch.text.chars().filter(|c| !c.is_whitespace()));
            }
            let orig_nows: String = f.text.chars().filter(|c| !c.is_whitespace()).collect();
            prop_assert_eq!(joined_nows, orig_nows);
        }
    }
}
