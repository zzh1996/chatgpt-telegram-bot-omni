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
    /// A server-side tool call started/progressed/finished.
    Tool(ToolCall),
    /// Citations attached to the answer text.
    Citations(Vec<Citation>),
    /// Refusal text.
    Refusal(String),
    /// Token usage / cost (may arrive more than once; the last one wins).
    Usage(Usage),
    /// The upstream finished. `reason` is normalized, `native` is the raw upstream reason.
    Finish { reason: Option<String>, native: Option<String> },
    /// A notice from the adapter/router that the user should see (e.g. a
    /// router-side transformation was detected). Shown in the bot info area.
    Notice(String),
}

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
    pub notices: Vec<String>,
    /// Native replay payload, maintained by the adapter.
    pub native: Option<NativeOutput>,
    pending_images: Vec<(Vec<u8>, String)>,
    last_reasoning_block: Option<usize>,
}

impl Accumulator {
    pub fn new() -> Self {
        Self::default()
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
                } else {
                    self.parts.push(Part::text(t));
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
                    return;
                }
                self.last_reasoning_block = Some(block);
                self.parts.push(Part::Reasoning(Reasoning { kind, text: Some(text), ..Default::default() }));
            }
            StreamEvent::OpaqueReasoning { kind } => {
                let already =
                    self.parts.iter().any(|p| matches!(p, Part::Reasoning(r) if r.kind == kind && r.text.is_none()));
                if !already {
                    self.parts.push(Part::Reasoning(Reasoning { kind, ..Default::default() }));
                }
            }
            StreamEvent::Image { data, mime } => self.pending_images.push((data, mime)),
            StreamEvent::Tool(call) => {
                // Update an existing call with the same id, else append.
                if let Some(id) = &call.id {
                    for p in self.parts.iter_mut().rev() {
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
                            return;
                        }
                    }
                }
                self.parts.push(Part::ToolCall(call));
            }
            StreamEvent::Citations(cs) => {
                // Attach to the last text part (create one if needed).
                if !matches!(self.parts.last(), Some(Part::Text { .. })) {
                    if let Some(Part::Text { citations, .. }) =
                        self.parts.iter_mut().rev().find(|p| matches!(p, Part::Text { .. }))
                    {
                        citations.extend(cs);
                        return;
                    }
                    self.parts.push(Part::text(""));
                }
                if let Some(Part::Text { citations, .. }) = self.parts.last_mut() {
                    citations.extend(cs);
                }
            }
            StreamEvent::Refusal(t) => {
                if let Some(Part::Refusal { text }) = self.parts.last_mut() {
                    text.push_str(&t);
                } else {
                    self.parts.push(Part::Refusal { text: t });
                }
            }
            StreamEvent::Usage(u) => self.usage = Some(u),
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
        }
    }

    /// Generated images not yet stored as blobs. The caller stores them and
    /// calls [`Accumulator::attach_image`] with the resulting reference.
    pub fn take_pending_images(&mut self) -> Vec<(Vec<u8>, String)> {
        std::mem::take(&mut self.pending_images)
    }

    pub fn attach_image(&mut self, blob: BlobRef) {
        self.parts.push(Part::Image { blob, filename: None, generated: true });
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
        self.native = Some(NativeOutput { dialect: dialect.into(), model: model.into(), payload });
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
