//! Incremental Server-Sent Events parser (WHATWG semantics).
//!
//! Feed arbitrary byte chunks; complete events come out. Handles `\n`, `\r\n`
//! and `\r` line endings, multi-line `data:`, comments (`: keep-alive`),
//! `event:`/`id:`/`retry:` fields, and UTF-8 split across chunks.

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseItem {
    Event(SseEvent),
    /// A comment line (without the leading `:`), e.g. OpenRouter's `OPENROUTER PROCESSING` keep-alive.
    Comment(String),
    /// A line that is not SSE but a bare JSON object (`{"error":…}` instead of
    /// an event, as Zhipu has sent inside HTTP-200 streams).
    Json(String),
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: String,
    has_data: bool,
    event: Option<String>,
    id: Option<String>,
    /// Last byte processed was `\r` (so a following `\n` must be swallowed).
    pending_cr: bool,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseItem> {
        let mut out = Vec::new();
        for &b in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.line(&mut out),
                b'\r' => {
                    self.line(&mut out);
                    self.pending_cr = true;
                }
                _ => self.buf.push(b),
            }
        }
        out
    }

    /// Flush at end of stream: a final event without a trailing blank line is dispatched.
    pub fn finish(&mut self) -> Vec<SseItem> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            self.line(&mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn line(&mut self, out: &mut Vec<SseItem>) {
        let raw = std::mem::take(&mut self.buf);
        let line = String::from_utf8_lossy(&raw);
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if let Some(c) = line.strip_prefix(':') {
            out.push(SseItem::Comment(c.strip_prefix(' ').unwrap_or(c).to_string()));
            return;
        }
        if line.starts_with('{') {
            out.push(SseItem::Json(line.to_string()));
            return;
        }
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_string()),
            "id" => self.id = Some(value.to_string()),
            _ => {} // `retry` and unknown fields are ignored
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseItem>) {
        if self.has_data || self.event.is_some() {
            out.push(SseItem::Event(SseEvent {
                event: self.event.take(),
                data: std::mem::take(&mut self.data),
                id: self.id.take(),
            }));
        }
        self.has_data = false;
        self.data.clear();
        self.event = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_json_lines_are_kept() {
        let mut p = SseParser::new();
        let items = p.feed(b"data: {\"a\":1}\n\n{\"error\":{\"code\":\"1113\",\"message\":\"x\"}}\n");
        assert_eq!(data(&items), vec!["{\"a\":1}".to_string()]);
        assert!(matches!(&items[1], SseItem::Json(l) if l.starts_with("{\"error\"")));
    }

    fn data(items: &[SseItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|i| match i {
                SseItem::Event(e) => Some(e.data.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn basic_and_comments() {
        let mut p = SseParser::new();
        let items = p.feed(b": OPENROUTER PROCESSING\n\ndata: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(items[0], SseItem::Comment("OPENROUTER PROCESSING".into()));
        assert_eq!(data(&items), vec!["{\"a\":1}", "[DONE]"]);
    }

    #[test]
    fn split_across_chunks_and_crlf() {
        let mut p = SseParser::new();
        let mut all = Vec::new();
        let s = "event: response.output_text.delta\r\ndata: {\"delta\":\"é中\"}\r\n\r\ndata: x\r\ndata: y\r\n\r\n";
        for b in s.as_bytes().chunks(3) {
            all.extend(p.feed(b));
        }
        all.extend(p.finish());
        let evs: Vec<_> = all.iter().filter_map(|i| if let SseItem::Event(e) = i { Some(e) } else { None }).collect();
        assert_eq!(evs[0].event.as_deref(), Some("response.output_text.delta"));
        assert_eq!(evs[0].data, "{\"delta\":\"é中\"}");
        assert_eq!(evs[1].data, "x\ny");
    }

    #[test]
    fn final_event_without_blank_line() {
        let mut p = SseParser::new();
        let mut items = p.feed(b"data: tail");
        items.extend(p.finish());
        assert_eq!(data(&items), vec!["tail"]);
    }

    #[test]
    fn lone_cr_line_endings() {
        let mut p = SseParser::new();
        let items = p.feed(b"data: a\r\rdata: b\r\r");
        assert_eq!(data(&items), vec!["a", "b"]);
    }
}
