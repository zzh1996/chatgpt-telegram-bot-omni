//! How OpenRouter really reports failures, from responses recorded on
//! 2026-11-11 (`tests/fixtures/errors/`, one request per case; no API is called
//! here). Each test pins down how the bot classifies the case: retried or not,
//! context overflow or not, and that the complete error object is kept.

use serde_json::Value;

use crate::Accumulator;
use crate::chatstream::{EventParser, http_error};
use crate::error::{LlmError, Retry};
use crate::fixtures::case;

/// The error the bot builds for a recorded HTTP error response.
fn http(name: &str) -> LlmError {
    let c = case(name);
    let status = c["status"].as_u64().unwrap() as u16;
    let retry_after = c["headers"]
        .as_object()
        .and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("retry-after")))
        .and_then(|(_, v)| v.as_str()?.parse().ok());
    let text = match &c["body"] {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let e = http_error(status, retry_after, &text, super::chat::error_from_json);
    assert_eq!(e.status, Some(status), "{name}");
    assert_eq!(e.details.as_ref(), Some(&c["body"]), "{name}: the whole body is kept");
    e
}

/// Feed a recorded stream to its dialect's parser: the events, or the in-band error.
fn stream(name: &str) -> (Accumulator, Result<(), LlmError>) {
    let c = case(name);
    let mut acc = Accumulator::new();
    let events = c["sse"].as_array().unwrap().clone();
    let run = |p: &mut dyn EventParser, acc: &mut Accumulator| -> Result<(), LlmError> {
        for e in &events {
            for ev in p.event(None, e)? {
                acc.apply(ev);
            }
        }
        assert!(p.finished(), "{name}: stream must end with a terminal event");
        Ok(())
    };
    let r = match c["endpoint"].as_str().unwrap() {
        "chat" => run(&mut super::chat::ChunkParser::default(), &mut acc),
        "responses" => run(&mut super::responses::EventParser::default(), &mut acc),
        "messages" => run(&mut super::messages::EventParser::default(), &mut acc),
        other => panic!("{other}"),
    };
    (acc, r)
}

#[test]
fn request_errors_are_not_retried() {
    for (name, needle) in [
        ("cc_bad_key", "User not found"),
        ("cc_unknown_model", "is not a valid model ID"),
        ("cc_no_endpoint", "No allowed providers are available"),
        ("cc_reasoning_mandatory_off", "Reasoning is mandatory"),
        ("cc_audio_to_text_model", "No endpoints found that support input audio"),
        ("msg_empty_message", "at least 1 token"),
        ("msg_bad_image_claude", "image"),
        ("cc_bad_image_gemini", "Provider returned error"),
    ] {
        let e = http(name);
        assert_eq!(e.retry, Retry::Never, "{name}: {e}");
        assert!(e.message.contains(needle), "{name}: {e}");
        assert!(!e.is_context_overflow(), "{name}");
    }
    // Gemini's own message is in `metadata.raw` (shown to users in full).
    let e = http("cc_bad_image_gemini");
    assert!(e.details.unwrap()["error"]["metadata"]["raw"].as_str().unwrap().contains("Unable to process input image"));
}

#[test]
fn not_enough_credits_is_not_retried() {
    for name in ["cc_cannot_afford", "rsp_cannot_afford"] {
        let e = http(name);
        assert_eq!(e.status, Some(402));
        assert_eq!(e.retry, Retry::Never, "{name}: {e}");
        assert!(e.message.contains("requires more credits"), "{name}: {e}");
    }
}

#[test]
fn context_overflow_is_recognised() {
    // OpenRouter rejects before calling the provider (nothing billed).
    for name in ["cc_context_too_long", "msg_context_too_long"] {
        let e = http(name);
        assert_eq!(e.status, Some(400));
        assert!(e.is_context_overflow(), "{name}: {e}");
        assert_eq!(e.retry, Retry::Never, "{name}: {e}");
    }
}

#[test]
fn upstream_rate_limit_waits_and_retries() {
    let e = http("cc_rate_limited_upstream");
    assert_eq!(e.retry, Retry::After(20), "{e}");
    let d = e.details.unwrap();
    assert_eq!(d["error"]["metadata"]["limit_source"], "upstream_provider_shared_pool");
}

#[test]
fn output_limit_is_a_normal_stop() {
    for name in ["cc_length", "rsp_length", "msg_length"] {
        let (acc, r) = stream(name);
        r.unwrap();
        assert_eq!(acc.finish_reason.as_deref(), Some("length"), "{name}");
        assert!(acc.has_output(), "{name}: the partial answer is kept");
    }
    let (acc, _) = stream("rsp_length");
    assert_eq!(acc.finish_details.as_ref().unwrap()["reason"], "max_output_tokens");
}

#[test]
fn reasoning_can_use_up_the_whole_budget() {
    let (acc, r) = stream("rsp_reasoning_ate_budget");
    r.unwrap();
    assert_eq!(acc.finish_reason.as_deref(), Some("length"));
    assert!(acc.parts().iter().all(|p| !matches!(p, omni_core::Part::Text { text, .. } if !text.is_empty())));
}

/// OpenRouter clamps an oversized `max_tokens` instead of rejecting it (the
/// official API returns 400). The bot never sends one, so this only documents it.
#[test]
fn oversized_max_tokens_is_silently_accepted() {
    let (acc, r) = stream("msg_max_tokens_too_big");
    r.unwrap();
    assert!(acc.has_output());
}

/// OpenAI's own reason (the image) is lost: OpenRouter reports an empty
/// provider response, which looks transient, so the bot retries it.
#[test]
fn openai_bad_image_arrives_as_an_opaque_provider_failure() {
    let (_, r) = stream("rsp_bad_image_openai");
    let e = r.unwrap_err();
    assert_eq!(e.error_type.as_deref(), Some("provider_unavailable"), "{e}");
    assert_eq!(e.retry, Retry::Backoff);
    assert!(e.message.contains("empty response"), "{e}");
    assert!(!e.is_context_overflow());
}

#[test]
fn every_recorded_case_has_a_test() {
    let dir = format!("{}/tests/fixtures/errors", env!("CARGO_MANIFEST_DIR"));
    let tested = [
        "cc_bad_key",
        "cc_unknown_model",
        "cc_no_endpoint",
        "cc_reasoning_mandatory_off",
        "cc_audio_to_text_model",
        "msg_empty_message",
        "msg_bad_image_claude",
        "cc_bad_image_gemini",
        "cc_cannot_afford",
        "rsp_cannot_afford",
        "cc_context_too_long",
        "msg_context_too_long",
        "cc_rate_limited_upstream",
        "cc_length",
        "rsp_length",
        "msg_length",
        "rsp_reasoning_ate_budget",
        "msg_max_tokens_too_big",
        "rsp_bad_image_openai",
    ];
    for f in std::fs::read_dir(dir).unwrap() {
        let name = f.unwrap().file_name().into_string().unwrap();
        let Some(stem) = name.strip_suffix(".json") else { continue };
        assert!(tested.contains(&stem), "recorded case without a test: {name}");
    }
}
