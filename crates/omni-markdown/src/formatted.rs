//! Formatted text: a string plus non-overlapping-or-nested formatting spans,
//! mirroring Telegram's `MessageEntity` model, plus split hints.
//!
//! Spans use *byte* offsets into `text` internally; conversion to Telegram's
//! UTF-16 offsets happens in [`Formatted::to_telegram`].

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntityKind {
    Bold,
    Italic,
    /// Reserved for bot-generated labels: the Markdown renderer never emits it,
    /// so model output can never look like a bot label (anti-spoofing).
    Underline,
    Strikethrough,
    Spoiler,
    Code,
    Pre {
        language: String,
    },
    TextUrl {
        url: String,
    },
    Blockquote {
        collapsed: bool,
    },
}

impl EntityKind {
    /// Entities that may not contain other entities.
    pub fn is_atomic(&self) -> bool {
        matches!(self, EntityKind::Code | EntityKind::Pre { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: EntityKind,
}

/// Where a message may be split. Higher is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BreakPriority {
    Char = 0,
    Space = 1,
    Line = 2,
    Block = 3,
    Section = 4,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Formatted {
    pub text: String,
    pub spans: Vec<Span>,
    /// Explicit break hints (byte position *before which* a split is allowed).
    /// Line/space breaks are derived automatically by the splitter.
    pub breaks: Vec<(usize, BreakPriority)>,
}

/// A Telegram entity with UTF-16 offsets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TgEntity {
    pub offset: i32,
    pub length: i32,
    pub kind: EntityKind,
}

pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

impl Formatted {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Default::default() }
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn utf16_len(&self) -> usize {
        utf16_len(&self.text)
    }

    pub fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }

    pub fn push_char(&mut self, c: char) {
        self.text.push(c);
    }

    /// Append text wrapped in a single entity.
    pub fn push_styled(&mut self, s: &str, kind: EntityKind) {
        let start = self.text.len();
        self.text.push_str(s);
        self.add_span(start, self.text.len(), kind);
    }

    pub fn add_span(&mut self, start: usize, end: usize, kind: EntityKind) {
        if start < end {
            self.spans.push(Span { start, end, kind });
        }
    }

    /// Wrap everything appended since `start` in `kind`.
    pub fn wrap_from(&mut self, start: usize, kind: EntityKind) {
        let end = self.text.len();
        self.add_span(start, end, kind);
    }

    pub fn mark_break(&mut self, prio: BreakPriority) {
        let pos = self.text.len();
        if let Some(last) = self.breaks.last_mut()
            && last.0 == pos
        {
            last.1 = last.1.max(prio);
            return;
        }
        self.breaks.push((pos, prio));
    }

    /// Append another formatted text, shifting its offsets.
    pub fn append(&mut self, other: &Formatted) {
        let base = self.text.len();
        self.text.push_str(&other.text);
        self.spans.extend(other.spans.iter().map(|s| Span {
            start: s.start + base,
            end: s.end + base,
            kind: s.kind.clone(),
        }));
        for &(p, pr) in &other.breaks {
            self.breaks.push((p + base, pr));
        }
    }

    /// Ensure the text ends with at least `n` newlines (unless empty).
    pub fn ensure_trailing_newlines(&mut self, n: usize) {
        if self.text.is_empty() {
            return;
        }
        let have = self.text.chars().rev().take_while(|&c| c == '\n').count();
        for _ in have..n {
            self.text.push('\n');
        }
    }

    /// Normalize spans so that Telegram accepts them:
    /// - drop empty spans, shrink spans to exclude leading/trailing whitespace;
    /// - remove entities nested inside `code`/`pre`;
    /// - flatten nested blockquotes; keep `pre` out of blockquotes;
    /// - sort outer-first.
    pub fn normalize(&mut self) {
        let text = &self.text;
        let mut spans: Vec<Span> = Vec::with_capacity(self.spans.len());
        for s in self.spans.drain(..) {
            let start = s.start.min(text.len());
            let end = s.end.min(text.len());
            if start >= end || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                continue;
            }
            let slice = &text[start..end];
            let lead = slice.len() - slice.trim_start().len();
            let trail = slice.len() - slice.trim_end().len();
            if lead + trail >= slice.len() {
                continue; // whitespace-only
            }
            let (start, end) = match s.kind {
                // Keep code blocks' leading indentation; only trim trailing whitespace/newlines.
                EntityKind::Pre { .. } => (start, end - trail),
                _ => (start + lead, end - trail),
            };
            spans.push(Span { start, end, kind: s.kind });
        }
        // Remove spans inside atomic spans (and partially overlapping ones, clip them out).
        let atomic: Vec<(usize, usize)> =
            spans.iter().filter(|s| s.kind.is_atomic()).map(|s| (s.start, s.end)).collect();
        let mut out: Vec<Span> = Vec::with_capacity(spans.len());
        for s in spans {
            if s.kind.is_atomic() {
                // Drop atomic spans nested in other atomic spans.
                if atomic.iter().any(|&(a, b)| (a, b) != (s.start, s.end) && a <= s.start && s.end <= b) {
                    continue;
                }
                out.push(s);
                continue;
            }
            let inside_atomic = atomic.iter().any(|&(a, b)| a <= s.start && s.end <= b);
            if inside_atomic {
                continue;
            }
            out.push(s);
        }
        // Blockquotes: merge/flatten nested ones and cut them around `pre` spans.
        let mut quotes: Vec<Span> = Vec::new();
        let mut others: Vec<Span> = Vec::new();
        for s in out {
            if matches!(s.kind, EntityKind::Blockquote { .. }) {
                quotes.push(s);
            } else {
                others.push(s);
            }
        }
        quotes.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        let mut flat: Vec<Span> = Vec::new();
        for q in quotes {
            if let Some(last) = flat.last_mut()
                && q.start < last.end
            {
                // nested or overlapping: extend the outer one
                if q.end > last.end {
                    last.end = q.end;
                }
                if let (EntityKind::Blockquote { collapsed: a }, EntityKind::Blockquote { collapsed: b }) =
                    (&mut last.kind, &q.kind)
                {
                    *a = *a || *b;
                }
                continue;
            }
            flat.push(q);
        }
        let pres: Vec<(usize, usize)> =
            others.iter().filter(|s| matches!(s.kind, EntityKind::Pre { .. })).map(|s| (s.start, s.end)).collect();
        for q in flat {
            let mut pieces = vec![(q.start, q.end)];
            for &(a, b) in &pres {
                let mut next = Vec::new();
                for (s, e) in pieces {
                    if b <= s || a >= e {
                        next.push((s, e));
                    } else {
                        if s < a {
                            next.push((s, a));
                        }
                        if b < e {
                            next.push((b, e));
                        }
                    }
                }
                pieces = next;
            }
            for (s, e) in pieces {
                let slice = &self.text[s..e];
                let lead = slice.len() - slice.trim_start().len();
                let trail = slice.len() - slice.trim_end().len();
                if lead + trail < slice.len() {
                    others.push(Span { start: s + lead, end: e - trail, kind: q.kind.clone() });
                }
            }
        }
        // Non-atomic spans that partially overlap an atomic span: clip to the part outside.
        let mut fixed = Vec::with_capacity(others.len());
        for s in others {
            if s.kind.is_atomic() {
                fixed.push(s);
                continue;
            }
            let mut ok = true;
            for &(a, b) in &atomic {
                let partial = (s.start < a && s.end > a && s.end < b) || (s.start > a && s.start < b && s.end > b);
                if partial {
                    ok = false;
                    break;
                }
            }
            if ok {
                fixed.push(s);
            }
        }
        fixed.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
        fixed.dedup();
        self.spans = fixed;
        self.breaks.sort_by_key(|b| b.0);
        self.breaks.dedup_by(|a, b| {
            if a.0 == b.0 {
                b.1 = b.1.max(a.1);
                true
            } else {
                false
            }
        });
    }

    /// Return a sub-range `[start, end)` (byte offsets) with spans clipped.
    pub fn slice(&self, start: usize, end: usize) -> Formatted {
        let text = self.text[start..end].to_string();
        let spans = self
            .spans
            .iter()
            .filter_map(|s| {
                let a = s.start.max(start);
                let b = s.end.min(end);
                (a < b).then(|| Span { start: a - start, end: b - start, kind: s.kind.clone() })
            })
            .collect();
        let breaks =
            self.breaks.iter().filter(|(p, _)| *p > start && *p < end).map(|&(p, pr)| (p - start, pr)).collect();
        Formatted { text, spans, breaks }
    }

    /// Trim leading/trailing whitespace, keeping spans consistent.
    pub fn trimmed(&self) -> Formatted {
        let start = self.text.len() - self.text.trim_start().len();
        let end = self.text.trim_end().len();
        if start >= end {
            return Formatted::default();
        }
        let mut f = self.slice(start, end);
        f.normalize();
        f
    }

    /// Convert to Telegram text + entities (UTF-16 offsets).
    pub fn to_telegram(&self) -> (String, Vec<TgEntity>) {
        let mut f = self.clone();
        f.normalize();
        let map = Utf16Map::new(&f.text);
        let entities = f
            .spans
            .iter()
            .map(|s| {
                let a = map.at(s.start);
                let b = map.at(s.end);
                TgEntity { offset: a as i32, length: (b - a) as i32, kind: s.kind.clone() }
            })
            .collect();
        (f.text, entities)
    }
}

/// Byte offset -> UTF-16 offset lookup for char boundaries.
pub struct Utf16Map {
    /// `table[i]` = utf16 offset of byte `i` if `i` is a char boundary.
    table: Vec<u32>,
}

impl Utf16Map {
    pub fn new(text: &str) -> Self {
        let mut table = vec![0u32; text.len() + 1];
        let mut u = 0u32;
        let mut last = 0usize;
        for (i, c) in text.char_indices() {
            for t in table.iter_mut().take(i + 1).skip(last) {
                *t = u;
            }
            u += c.len_utf16() as u32;
            last = i + 1;
        }
        for t in table.iter_mut().skip(last) {
            *t = u;
        }
        table[text.len()] = u;
        Self { table }
    }

    pub fn at(&self, byte: usize) -> usize {
        self.table[byte] as usize
    }

    pub fn len_between(&self, a: usize, b: usize) -> usize {
        self.at(b) - self.at(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_offsets() {
        let mut f = Formatted::new();
        f.push_str("😀 a");
        f.push_styled("bold", EntityKind::Bold);
        let (t, e) = f.to_telegram();
        assert_eq!(t, "😀 abold");
        assert_eq!(e[0].offset, 4); // emoji = 2 code units, space, 'a'
        assert_eq!(e[0].length, 4);
    }

    #[test]
    fn whitespace_shrink_and_drop() {
        let mut f = Formatted::plain("a   b");
        f.add_span(1, 4, EntityKind::Bold); // whitespace only
        f.add_span(0, 3, EntityKind::Italic); // "a  " -> "a"
        f.normalize();
        assert_eq!(f.spans, vec![Span { start: 0, end: 1, kind: EntityKind::Italic }]);
    }

    #[test]
    fn nothing_inside_code() {
        let mut f = Formatted::plain("abc def");
        f.add_span(0, 7, EntityKind::Code);
        f.add_span(1, 2, EntityKind::Bold);
        f.normalize();
        assert_eq!(f.spans.len(), 1);
    }

    #[test]
    fn quote_cut_around_pre() {
        let mut f = Formatted::plain("q1\ncode\nq2");
        f.add_span(0, 10, EntityKind::Blockquote { collapsed: false });
        f.add_span(3, 7, EntityKind::Pre { language: String::new() });
        f.normalize();
        let quotes: Vec<_> = f.spans.iter().filter(|s| matches!(s.kind, EntityKind::Blockquote { .. })).collect();
        assert_eq!(quotes.len(), 2);
        assert_eq!(&f.text[quotes[0].start..quotes[0].end], "q1");
        assert_eq!(&f.text[quotes[1].start..quotes[1].end], "q2");
    }

    #[test]
    fn nested_quotes_flatten() {
        let mut f = Formatted::plain("abcdef");
        f.add_span(0, 6, EntityKind::Blockquote { collapsed: false });
        f.add_span(2, 4, EntityKind::Blockquote { collapsed: true });
        f.normalize();
        assert_eq!(f.spans.len(), 1);
        assert_eq!(f.spans[0].kind, EntityKind::Blockquote { collapsed: true });
    }
}
