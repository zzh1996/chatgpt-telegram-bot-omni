//! Normalized streaming events produced by provider adapters, and the
//! accumulator that turns them into an assistant [`Turn`].

use omni_core::content::{Citation, NativeOutput, Part, Reasoning, ReasoningKind, Role, ToolCall, Turn};
use omni_core::{BlobRef, Origin, Usage};
use serde_json::Value;

/// Something the adapter observed in the stream (already parsed).
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Routing/identity info (generation id, serving provider, concrete model).
    Meta { upstream_id: Option<String>, provider: Option<String>, model: Option<String> },
    /// Visible answer text.
    Text(String),
    /// Reasoning text for display (raw thinking or summary). `block` separates
    /// distinct reasoning blocks/summary parts.
    Reasoning { kind: ReasoningKind, text: String, block: usize },
    /// The model reported opaque/encrypted or redacted reasoning (nothing to show).
    OpaqueReasoning { kind: ReasoningKind },
    /// A generated image (already decoded).
    Image { data: Vec<u8>, mime: String },
    /// A draft image made while thinking (Gemini's `thought` image parts):
    /// shown with the reasoning.
    ThoughtImage { data: Vec<u8>, mime: String },
    /// Another generated file (a video, a file a code tool wrote), already
    /// downloaded: shown as a document where it appeared.
    File { data: Vec<u8>, mime: String, name: Option<String> },
    /// A preview of an image still being generated (streamed partial images):
    /// replaces the previous preview of the same `slot` (a stable name the
    /// provider picks, e.g. the output index); shown until `PreviewDone` for
    /// that slot or the end of the attempt, never kept in the turn.
    PartialImage { data: Vec<u8>, mime: String, slot: String },
    /// The image of `slot` is finished (its `Image` comes with it): its preview goes.
    PreviewDone { slot: String },
    /// A server-side tool call started/progressed/finished.
    Tool(ToolCall),
    /// A tool call the provider reports only after the answer text it grounded
    /// (Gemini's search and Maps grounding arrive with the last chunk): placed
    /// before that text, where it ran.
    ToolBefore(ToolCall),
    /// Citations attached to the answer text.
    Citations(Vec<Citation>),
    /// Refusal text.
    Refusal(String),
    /// Token usage / cost (may arrive more than once; the last one wins).
    Usage(Usage),
    /// Usage known so far, for display only (e.g. input tokens at the start of a
    /// stream). Never billed: the call's final [`StreamEvent::Usage`] replaces it.
    LiveUsage(Usage),
    /// The upstream finished. `reason` is normalized, `native` is the raw upstream reason.
    Finish { reason: Option<String>, native: Option<String> },
    /// The provider's own account of why the output stopped, verbatim (e.g.
    /// Anthropic `stop_details` of a refusal, Responses `incomplete_details`).
    FinishDetails(Value),
    /// A notice from the adapter/router that the user should see (e.g. a
    /// router-side transformation was detected). Shown in the bot info area.
    Notice(String),
    /// A resumable checkpoint (e.g. a Gemini long-decode continuation item):
    /// generation can be resumed from here after a drop or restart. `Null`
    /// clears it: the call can no longer be resumed (e.g. its job failed for
    /// good), so an error after it is retried as a new attempt, not resumed.
    Checkpoint(Value),
    /// The provider sends this call's whole output again, recovered from a stored
    /// copy after the stream broke (OpenAI `store: true`): what the call produced
    /// so far is discarded (see [`Accumulator::begin_call`]).
    Restart,
    /// The conversation's exact size in tokens after this call (its input plus
    /// output), when the API reports it (xAI `usage.context_details`): the next
    /// turn's context, also after server-side tools.
    ContextSize(u64),
}

/// The latest preview of an image still being generated: (slot, blob, pixel size).
pub type Preview = (String, BlobRef, Option<(u32, u32)>);

/// Builds an assistant turn incrementally. Rendering reads `turn()` at any time.
#[derive(Debug, Clone, Default)]
pub struct Accumulator {
    parts: Vec<Part>,
    pub upstream_id: Option<String>,
    pub provider: Option<String>,
    pub served_model: Option<String>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    pub native_finish_reason: Option<String>,
    /// See [`StreamEvent::FinishDetails`].
    pub finish_details: Option<Value>,
    pub notices: Vec<String>,
    /// Native replay payload, maintained by the adapter.
    pub native: Option<NativeOutput>,
    /// See [`StreamEvent::ContextSize`].
    pub context_size: Option<u64>,
    /// The parts when the current upstream call began ([`StreamEvent::Restart`]).
    call_start: Vec<Part>,
    /// Images to store: (bytes, MIME type, made while thinking).
    pending_images: Vec<(Vec<u8>, String, bool)>,
    /// Other generated files to store: (bytes, MIME type, file name).
    pending_files: Vec<(Vec<u8>, String, Option<String>)>,
    /// Image previews to store: (slot, bytes, MIME type).
    pending_previews: Vec<(String, Vec<u8>, String)>,
    /// Images still being generated, as their latest preview: (slot, blob,
    /// pixel size). Shown after the parts; not part of the turn.
    pub previews: Vec<Preview>,
    last_reasoning_block: Option<usize>,
    /// Parts changed since the last [`Accumulator::sync_parts`].
    changes: Option<PartsChange>,
}

/// Which parts changed since a mirror of [`Accumulator::parts`] was last synced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PartsChange {
    /// Parts before this index are unchanged.
    from: usize,
    /// The only change is text appended to part `from`, which existed before.
    append_only: bool,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Part `i` changed in any way (or was added).
    fn touched(&mut self, i: usize) {
        let from = self.changes.map_or(i, |c| c.from.min(i));
        self.changes = Some(PartsChange { from, append_only: false });
    }

    /// Text was appended to the existing part `i`.
    fn appended(&mut self, i: usize) {
        self.changes = Some(match self.changes {
            None => PartsChange { from: i, append_only: true },
            Some(c) if c.from == i => c,
            Some(c) => PartsChange { from: c.from.min(i), append_only: false },
        });
    }

    fn push_part(&mut self, p: Part) {
        self.parts.push(p);
        self.touched(self.parts.len() - 1);
    }

    /// A full copy of `parts()` was just taken: changes so far are in it.
    pub fn mark_synced(&mut self) {
        self.changes = None;
    }

    /// Bring `mirror` (a copy of `parts()` made earlier) up to date, copying
    /// only what changed since: usually just the text appended to the last part.
    pub fn sync_parts(&mut self, mirror: &mut Vec<Part>) {
        let Some(c) = self.changes.take() else { return };
        let parts = &self.parts;
        if c.append_only && mirror.len() == parts.len() && c.from < parts.len() {
            let grown = |m: &mut String, a: &String| {
                if a.len() >= m.len() && a.is_char_boundary(m.len()) {
                    m.push_str(&a[m.len()..]);
                    true
                } else {
                    false
                }
            };
            let ok = match (&mut mirror[c.from], &parts[c.from]) {
                (Part::Text { text: m, .. }, Part::Text { text: a, .. }) => grown(m, a),
                (Part::Refusal { text: m }, Part::Refusal { text: a }) => grown(m, a),
                (Part::Reasoning(m), Part::Reasoning(a)) => match (&mut m.text, &a.text) {
                    (Some(m), Some(a)) => grown(m, a),
                    _ => false,
                },
                _ => false,
            };
            if ok {
                return;
            }
        }
        let from = c.from.min(mirror.len());
        mirror.truncate(from);
        mirror.extend_from_slice(&parts[from..]);
    }

    pub fn apply(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Meta { upstream_id, provider, model } => {
                if upstream_id.is_some() {
                    self.upstream_id = upstream_id;
                }
                if provider.is_some() {
                    self.provider = provider;
                }
                if model.is_some() {
                    self.served_model = model;
                }
            }
            StreamEvent::Text(t) => {
                if t.is_empty() {
                    return;
                }
                if let Some(Part::Text { text, .. }) = self.parts.last_mut() {
                    text.push_str(&t);
                    self.appended(self.parts.len() - 1);
                } else {
                    self.push_part(Part::text(t));
                }
            }
            StreamEvent::Reasoning { kind, text, block } => {
                if text.is_empty() {
                    return;
                }
                let same_block = self.last_reasoning_block == Some(block);
                if let (true, Some(Part::Reasoning(r))) = (same_block, self.parts.last_mut())
                    && r.kind == kind
                {
                    r.text.get_or_insert_with(String::new).push_str(&text);
                    self.appended(self.parts.len() - 1);
                    return;
                }
                self.last_reasoning_block = Some(block);
                self.push_part(Part::Reasoning(Reasoning { kind, text: Some(text), ..Default::default() }));
            }
            StreamEvent::OpaqueReasoning { kind } => {
                let already =
                    self.parts.iter().any(|p| matches!(p, Part::Reasoning(r) if r.kind == kind && r.text.is_none()));
                if !already {
                    self.push_part(Part::Reasoning(Reasoning { kind, ..Default::default() }));
                }
            }
            StreamEvent::Image { data, mime } => self.pending_images.push((data, mime, false)),
            StreamEvent::ThoughtImage { data, mime } => self.pending_images.push((data, mime, true)),
            StreamEvent::File { data, mime, name } => self.pending_files.push((data, mime, name)),
            StreamEvent::PartialImage { data, mime, slot } => {
                // Only the newest preview of a slot matters.
                self.pending_previews.retain(|(s, ..)| *s != slot);
                self.pending_previews.push((slot, data, mime));
            }
            StreamEvent::PreviewDone { slot } => {
                self.pending_previews.retain(|(s, ..)| *s != slot);
                self.previews.retain(|(s, ..)| *s != slot);
            }
            StreamEvent::Restart => {
                let start = self.call_start.clone();
                self.restore_parts(start);
                self.pending_images.clear();
                self.pending_files.clear();
                self.pending_previews.clear();
                self.previews.clear();
                self.finish_reason = None;
                self.native_finish_reason = None;
                self.finish_details = None;
            }
            StreamEvent::ContextSize(n) => self.context_size = Some(n),
            StreamEvent::Tool(call) => {
                // Update an existing call with the same id, else append.
                if let Some(id) = &call.id {
                    for (i, p) in self.parts.iter_mut().enumerate().rev() {
                        if let Part::ToolCall(c) = p
                            && c.id.as_ref() == Some(id)
                        {
                            if call.status.is_some() {
                                c.status = call.status.clone();
                            }
                            if call.input.is_some() {
                                c.input = call.input.clone();
                            }
                            if call.output.is_some() {
                                c.output = call.output.clone();
                            }
                            self.touched(i);
                            return;
                        }
                    }
                }
                self.push_part(Part::ToolCall(call));
            }
            StreamEvent::ToolBefore(call) => {
                // Already shown: update it in place.
                if call.id.is_some() && self.parts.iter().any(|p| matches!(p, Part::ToolCall(c) if c.id == call.id)) {
                    return self.apply(StreamEvent::Tool(call));
                }
                // Before the trailing run of answer text (after any reasoning).
                let mut at = self.parts.len();
                while at > 0 && matches!(self.parts[at - 1], Part::Text { .. }) {
                    at -= 1;
                }
                self.parts.insert(at, Part::ToolCall(call));
                self.touched(at);
            }
            StreamEvent::Citations(cs) => {
                // Attach to the last text part (create one if needed).
                if !matches!(self.parts.last(), Some(Part::Text { .. })) {
                    if let Some(i) = self.parts.iter().rposition(|p| matches!(p, Part::Text { .. })) {
                        if let Part::Text { citations, .. } = &mut self.parts[i] {
                            citations.extend(cs);
                        }
                        self.touched(i);
                        return;
                    }
                    self.push_part(Part::text(""));
                }
                if let Some(Part::Text { citations, .. }) = self.parts.last_mut() {
                    citations.extend(cs);
                    self.touched(self.parts.len() - 1);
                }
            }
            StreamEvent::Refusal(t) => {
                if let Some(Part::Refusal { text }) = self.parts.last_mut() {
                    text.push_str(&t);
                    self.appended(self.parts.len() - 1);
                } else {
                    self.push_part(Part::Refusal { text: t });
                }
            }
            StreamEvent::Usage(u) => self.usage = Some(u),
            StreamEvent::LiveUsage(_) => {}
            StreamEvent::FinishDetails(d) => self.finish_details = Some(d),
            StreamEvent::Finish { reason, native } => {
                if reason.is_some() {
                    self.finish_reason = reason;
                }
                if native.is_some() {
                    self.native_finish_reason = native;
                }
            }
            StreamEvent::Notice(n) => {
                if !self.notices.contains(&n) {
                    self.notices.push(n);
                }
            }
            StreamEvent::Checkpoint(_) => {}
        }
    }

    /// Replace the accumulated parts (resume from a checkpoint snapshot).
    /// A new upstream call begins: [`StreamEvent::Restart`] returns to this point.
    pub fn begin_call(&mut self) {
        self.call_start = self.parts.clone();
    }

    pub fn restore_parts(&mut self, parts: Vec<Part>) {
        self.parts = parts;
        self.last_reasoning_block = None;
        self.touched(0);
    }

    /// Generated images not yet stored as blobs. The caller stores them and
    /// calls [`Accumulator::attach_image`] with the resulting reference.
    pub fn take_pending_images(&mut self) -> Vec<(Vec<u8>, String, bool)> {
        std::mem::take(&mut self.pending_images)
    }

    /// Image previews not yet stored as blobs (see [`Accumulator::set_preview`]).
    pub fn take_pending_previews(&mut self) -> Vec<(String, Vec<u8>, String)> {
        std::mem::take(&mut self.pending_previews)
    }

    /// The latest preview of `slot` (replacing the one before).
    pub fn set_preview(&mut self, slot: String, blob: BlobRef, dims: Option<(u32, u32)>) {
        match self.previews.iter_mut().find(|(s, ..)| *s == slot) {
            Some(p) => *p = (slot, blob, dims),
            None => self.previews.push((slot, blob, dims)),
        }
    }

    /// Generated files not yet stored as blobs (see [`Accumulator::attach_file`]).
    pub fn take_pending_files(&mut self) -> Vec<(Vec<u8>, String, Option<String>)> {
        std::mem::take(&mut self.pending_files)
    }

    pub fn attach_file(&mut self, blob: BlobRef, filename: Option<String>) {
        self.push_part(Part::File { blob, filename, generated: true });
    }

    /// `thought`: a draft made while thinking.
    pub fn attach_image(&mut self, blob: BlobRef, dims: Option<(u32, u32)>, thought: bool) {
        self.push_part(Part::Image { blob, filename: None, generated: true, dims, thought });
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    pub fn has_output(&self) -> bool {
        self.parts.iter().any(|p| match p {
            Part::Text { text, .. } => !text.is_empty(),
            _ => true,
        })
    }

    pub fn turn(&self) -> Turn {
        Turn { role: Role::Assistant, origin: Origin::Live, parts: self.parts.clone(), native: self.native.clone() }
    }

    /// Set (or replace) the native payload kept for replay.
    pub fn set_native(&mut self, dialect: &str, model: &str, payload: Value) {
        self.native =
            Some(NativeOutput { dialect: dialect.into(), model: model.into(), payload, context_tokens: None });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_text_and_reasoning_blocks() {
        let mut a = Accumulator::new();
        a.apply(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "A".into(), block: 0 });
        a.apply(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "B".into(), block: 0 });
        a.apply(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "C".into(), block: 1 });
        a.apply(StreamEvent::Text("hel".into()));
        a.apply(StreamEvent::Text("lo".into()));
        let t = a.turn();
        assert_eq!(t.parts.len(), 3);
        assert!(matches!(&t.parts[0], Part::Reasoning(r) if r.text.as_deref() == Some("AB")));
        assert!(matches!(&t.parts[1], Part::Reasoning(r) if r.text.as_deref() == Some("C")));
        assert_eq!(t.answer_text(), "hello");
    }

    #[test]
    fn sync_parts_keeps_a_mirror_current() {
        let mut a = Accumulator::new();
        let mut mirror = Vec::new();
        let check = |a: &mut Accumulator, mirror: &mut Vec<Part>| {
            a.sync_parts(mirror);
            assert_eq!(serde_json::to_value(&*mirror).unwrap(), serde_json::to_value(a.parts()).unwrap());
        };
        a.apply(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "think".into(), block: 0 });
        check(&mut a, &mut mirror);
        a.apply(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "ing".into(), block: 0 });
        check(&mut a, &mut mirror);
        a.apply(StreamEvent::Tool(ToolCall { id: Some("t".into()), name: "web_search".into(), ..Default::default() }));
        a.apply(StreamEvent::Text("Hel".into()));
        check(&mut a, &mut mirror);
        a.apply(StreamEvent::Text("lo 世界".into()));
        a.apply(StreamEvent::Text("!".into()));
        check(&mut a, &mut mirror);
        // An earlier part changes after text was appended to a later one.
        a.apply(StreamEvent::Tool(ToolCall {
            id: Some("t".into()),
            name: "web_search".into(),
            status: Some("completed".into()),
            ..Default::default()
        }));
        a.apply(StreamEvent::Text(" more".into()));
        check(&mut a, &mut mirror);
        a.apply(StreamEvent::Citations(vec![]));
        check(&mut a, &mut mirror);
        a.restore_parts(vec![Part::text("restored")]);
        check(&mut a, &mut mirror);
        a.sync_parts(&mut mirror); // no changes: nothing to do
        check(&mut a, &mut mirror);
    }

    #[test]
    fn grounding_reported_late_goes_before_the_answer() {
        let mut a = Accumulator::new();
        let mut mirror = Vec::new();
        a.apply(StreamEvent::Reasoning { kind: ReasoningKind::Summary, text: "think".into(), block: 0 });
        a.apply(StreamEvent::Text("Paris is ".into()));
        a.sync_parts(&mut mirror);
        a.apply(StreamEvent::Text("the capital.".into()));
        let call = ToolCall { id: Some("g".into()), name: "google_search".into(), ..Default::default() };
        a.apply(StreamEvent::ToolBefore(call.clone()));
        let kinds: Vec<&str> = a
            .parts()
            .iter()
            .map(|p| match p {
                Part::Reasoning(_) => "reasoning",
                Part::ToolCall(_) => "tool",
                Part::Text { .. } => "text",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["reasoning", "tool", "text"]);
        assert_eq!(a.turn().answer_text(), "Paris is the capital.");
        // A mirror taken before the insertion catches up.
        a.sync_parts(&mut mirror);
        assert_eq!(serde_json::to_value(&mirror).unwrap(), serde_json::to_value(a.parts()).unwrap());
        // Reported again (e.g. updated metadata): updated in place.
        a.apply(StreamEvent::ToolBefore(ToolCall { status: Some("completed".into()), ..call }));
        assert_eq!(a.parts().len(), 3);
    }

    #[test]
    fn image_previews_replace_each_other_and_go() {
        let mut a = Accumulator::new();
        let blob = |x: &[u8]| BlobRef::for_bytes(x, "image/png");
        a.apply(StreamEvent::PartialImage { data: b"p1".to_vec(), mime: "image/png".into(), slot: "0".into() });
        a.apply(StreamEvent::PartialImage { data: b"p2".to_vec(), mime: "image/png".into(), slot: "0".into() });
        let pending = a.take_pending_previews();
        assert_eq!(pending.len(), 1, "only the newest preview of a slot is stored");
        a.set_preview("0".into(), blob(b"p2"), None);
        a.set_preview("1".into(), blob(b"q1"), None);
        a.set_preview("0".into(), blob(b"p3"), None);
        assert_eq!(a.previews.len(), 2);
        assert_eq!(a.previews[0].1, blob(b"p3"));
        a.apply(StreamEvent::PreviewDone { slot: "0".into() });
        assert_eq!(a.previews.iter().map(|(s, ..)| s.as_str()).collect::<Vec<_>>(), ["1"]);
        assert!(a.parts().is_empty(), "previews are never parts of the turn");
    }

    #[test]
    fn tool_call_updates_in_place() {
        let mut a = Accumulator::new();
        a.apply(StreamEvent::Tool(ToolCall {
            id: Some("1".into()),
            name: "web_search".into(),
            status: Some("in_progress".into()),
            ..Default::default()
        }));
        a.apply(StreamEvent::Tool(ToolCall {
            id: Some("1".into()),
            name: "web_search".into(),
            status: Some("completed".into()),
            ..Default::default()
        }));
        assert_eq!(a.parts().len(), 1);
        assert!(matches!(&a.parts()[0], Part::ToolCall(c) if c.status.as_deref() == Some("completed")));
    }
}
