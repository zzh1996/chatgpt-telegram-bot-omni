//! Output token budget (`max_tokens`) selection without extra latency.
//!
//! Goal: let the model write as much as it can, without failing because
//! `prompt + max_tokens` exceeds the context window. Exact prompt token counts
//! are not available before the request (tokenizers differ and are not all
//! public), so:
//!
//! 1. the exact size of the conversation up to the latest assistant turn of the
//!    same model is taken from that turn's reported usage (`context_tokens`);
//! 2. only the newer turns are *estimated*, conservatively (over-estimating
//!    merely lowers `max_tokens` when the context is nearly full);
//! 3. `max_tokens = min(model max output, cap, context − prompt − margin)`;
//! 4. if the upstream still reports a context overflow, the caller retries
//!    immediately with a smaller value ([`shrink_after_overflow`]).

use omni_core::Turn;
use omni_core::content::{Part, Role};

/// Exact token count of `history[..upto]` as reported by the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownContext {
    pub upto: usize,
    pub tokens: u64,
}

/// Conservative token estimate of one turn (text by character class, media by size).
pub fn estimate_turn(t: &Turn) -> u64 {
    let mut n: u64 = 8; // per-message overhead
    for p in &t.parts {
        n += match p {
            Part::Text { text, .. } => estimate_text(text),
            Part::Reasoning(r) => {
                r.text.as_deref().map(estimate_text).unwrap_or(0) + r.opaque.as_ref().map_or(0, |o| o.len() as u64 / 4)
            }
            Part::Image { .. } => 2_500,
            Part::File { blob, .. } => {
                let m = blob.mime.as_str();
                let b = blob.size;
                if m.starts_with("audio/") {
                    b / 400
                } else if m.starts_with("video/") {
                    b / 500
                } else if m == "application/pdf" {
                    (b / 20).max(1_000)
                } else {
                    // text-like files are inlined: estimate as text bytes
                    b / 2
                }
            }
            Part::RemoteMedia { .. } => 20_000,
            Part::ToolCall(c) => {
                c.input.as_ref().map_or(0, |v| v.to_string().len() as u64 / 3)
                    + c.output.as_ref().map_or(0, |v| v.to_string().len() as u64 / 3)
            }
            Part::Refusal { text } => estimate_text(text),
        };
    }
    if t.role == Role::System {
        n += 8;
    }
    n
}

/// Tokens for text: ~3.3 ASCII characters per token, ~1 token per other
/// character (CJK etc.), rounded up — deliberately pessimistic.
pub fn estimate_text(s: &str) -> u64 {
    let mut ascii = 0u64;
    let mut other = 0u64;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(3) + other + other / 5
}

/// Estimated prompt tokens for `history`, using exact counts where known.
pub fn estimate_prompt(history: &[Turn], known: Option<KnownContext>) -> u64 {
    let (start, base) = match known {
        Some(k) if k.upto <= history.len() => (k.upto, k.tokens),
        _ => (0, 0),
    };
    base + history[start..].iter().map(estimate_turn).sum::<u64>() + 16
}

/// Choose `max_tokens`. Returns `None` when nothing is known about the model
/// (the provider default then applies).
pub fn choose(
    context_length: Option<u64>,
    model_max: Option<u64>,
    cap: Option<u64>,
    prompt_estimate: u64,
) -> Option<u64> {
    let mut v = model_max?;
    if let Some(c) = cap {
        v = v.min(c);
    }
    if let Some(ctx) = context_length {
        let margin = (ctx / 50).max(1_024);
        let avail = ctx.saturating_sub(prompt_estimate).saturating_sub(margin);
        v = v.min(avail);
    }
    Some(v.max(1_024))
}

/// After a context-overflow error, pick a smaller budget. Recognizes the
/// common message shapes (`maximum context length is X ... (Y in the
/// messages`, `prompt is too long: Y tokens > X maximum`); otherwise halves.
/// Returns `None` when retrying cannot help (the prompt alone is too long) or
/// the budget would become tiny.
pub fn shrink_after_overflow(previous: u64, message: &str) -> Option<u64> {
    let num_after = |pat: &str| -> Option<u64> {
        let i = message.find(pat)? + pat.len();
        let digits: String = message[i..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ',')
            .filter(|c| *c != ',')
            .collect();
        digits.parse().ok()
    };
    let num_before = |pat: &str| -> Option<u64> {
        let i = message.find(pat)?;
        let head = message[..i].trim_end();
        let digits: String =
            head.chars().rev().take_while(|c| c.is_ascii_digit() || *c == ',').filter(|c| *c != ',').collect();
        digits.chars().rev().collect::<String>().parse().ok()
    };
    let limit = num_after("maximum context length is")
        .or_else(|| num_after("context window of"))
        .or_else(|| num_after("context length of"))
        .or_else(|| num_before("maximum"));
    let prompt = num_before(" in the messages")
        .or_else(|| num_after("prompt is too long:"))
        .or_else(|| num_before("input tokens"));
    let v = match (limit, prompt) {
        (Some(l), Some(p)) if p >= l => return None, // the prompt alone does not fit
        (Some(l), Some(p)) => l - p - (l / 100).max(256),
        _ => previous / 2,
    };
    (v >= 256 && v < previous).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_estimates_are_pessimistic() {
        // English: real ~4 chars/token
        assert!(estimate_text("hello world, this is a test sentence.") >= 9);
        // Chinese: real ~1-1.5 chars/token
        assert!(estimate_text("你好世界这是一个测试") >= 10);
    }

    #[test]
    fn known_context_is_used() {
        let h = vec![Turn::user_text("a".repeat(30000)), Turn::assistant_text("b"), Turn::user_text("short")];
        let est_all = estimate_prompt(&h, None);
        let est_known = estimate_prompt(&h, Some(KnownContext { upto: 2, tokens: 7_000 }));
        assert!(est_all > 10_000);
        assert!(est_known < 7_100);
    }

    #[test]
    fn choose_respects_context() {
        // Grok-like: 500K context, 450K max out; 100K prompt => ~390K
        let v = choose(Some(500_000), Some(450_000), Some(393_216), 100_000).unwrap();
        assert!(v <= 400_000 - 10_000 && v > 380_000, "{v}");
        // Small prompt: capped by the cap
        assert_eq!(choose(Some(1_000_000), Some(128_000), Some(393_216), 50), Some(128_000));
        // Nearly full context: floor
        assert_eq!(choose(Some(65_536), Some(58_982), None, 70_000), Some(1_024));
        assert_eq!(choose(None, None, Some(1), 0), None);
    }

    #[test]
    fn shrink_parses_openai_style_messages() {
        let msg = "This model's maximum context length is 131072 tokens. However, you requested 140000 tokens (20000 in the messages, 120000 in the completion).";
        let v = shrink_after_overflow(120_000, msg).unwrap();
        assert!(v + 20_000 <= 131_072 && v > 105_000, "{v}");
        assert_eq!(shrink_after_overflow(64_000, "prompt is too long: 210000 tokens > 200000 maximum"), None);
        assert_eq!(shrink_after_overflow(1000, "context too long"), Some(500));
        assert_eq!(shrink_after_overflow(300, "x"), None);
    }
}
