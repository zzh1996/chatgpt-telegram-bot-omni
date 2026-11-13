//! Request building, stream parsing (synthetic event sequences written per the
//! documented formats, plus recorded Anthropic event shapes), cost and error
//! classification of the Anthropic provider. No real API is called; the HTTP
//! tests use a local fake server.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{Citation, NativeOutput, Part, ReasoningKind, Role, ToolCall, Turn};
use omni_core::trigger::OptionToken;
use omni_core::usage::CostSource;
use omni_core::{BlobRef, Origin};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::stream::{Parser, error_from_json};
use super::*;
use crate::chatstream::http_error;
use crate::error::{ErrorKind, Retry};
use crate::{Accumulator, BoxFuture, NullObserver};

// ------------------------------------------------------------------ helpers

#[derive(Default)]
struct Blobs(HashMap<String, Vec<u8>>);

impl Blobs {
    fn add(&mut self, bytes: &[u8], mime: &str) -> BlobRef {
        let r = BlobRef::for_bytes(bytes, mime);
        self.0.insert(r.sha256.clone(), bytes.to_vec());
        r
    }
}

impl BlobLoader for Blobs {
    fn load<'a>(&'a self, r: &'a BlobRef) -> BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async move { self.0.get(&r.sha256).cloned().ok_or_else(|| anyhow::anyhow!("no blob {r}")) })
    }
}

fn provider_at(base_url: &str, cap: Option<u64>) -> Anthropic {
    Anthropic::new(DirectConfig {
        api_key: "sk-ant-test".into(),
        base_url: base_url.into(),
        connect_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: cap,
        cny_per_usd: 7.1,
        long_decode_budget: 393_216,
    })
    .unwrap()
}

fn provider() -> Anthropic {
    provider_at("https://api.anthropic.com/v1", None)
}

fn opts(s: &[&str]) -> Vec<OptionToken> {
    s.iter()
        .map(|o| match o.split_once('=') {
            Some((k, v)) => OptionToken::KeyValue(k.into(), v.into()),
            None => OptionToken::Flag(o.to_string()),
        })
        .collect()
}

fn input<'a>(model: &'a str, options: &'a [OptionToken], history: &'a [Turn]) -> CallInput<'a> {
    CallInput {
        model,
        options,
        history,
        session_id: Some("tg-1".into()),
        user: None,
        known_context: None,
        max_tokens_override: None,
    }
}

async fn prep(model: &str, o: &[&str], history: &[Turn]) -> Result<Prepared, LlmError> {
    let o = opts(o);
    provider().prepare(&input(model, &o, history), &[], &Blobs::default()).await
}

async fn prep_ok(model: &str, o: &[&str]) -> Prepared {
    prep(model, o, &[Turn::user_text("hi")]).await.unwrap_or_else(|e| panic!("{model} {o:?}: {e}"))
}

async fn prep_err(model: &str, o: &[&str]) -> String {
    match prep(model, o, &[Turn::user_text("hi")]).await {
        Ok(p) => panic!("{model} {o:?}: expected an error, got {}", p.body),
        Err(e) => {
            assert_eq!(e.retry, Retry::Never);
            e.message
        }
    }
}

fn assistant(model: &str, payload: Value, text: &str) -> Turn {
    Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::text(text)],
        native: Some(NativeOutput { dialect: DIALECT.into(), model: model.into(), payload, context_tokens: None }),
    }
}

fn system(text: &str) -> Turn {
    Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text(text)], native: None }
}

fn feed(p: &mut Parser, events: &[Value]) -> Vec<StreamEvent> {
    let mut out = Vec::new();
    for e in events {
        out.extend(p.event(None, e).unwrap_or_else(|err| panic!("{e}: {err}")));
    }
    out
}

fn accumulate(events: &[StreamEvent]) -> Accumulator {
    let mut acc = Accumulator::new();
    for e in events {
        acc.apply(e.clone());
    }
    acc
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

fn message_start(model: &str, usage: Value) -> Value {
    json!({"type": "message_start", "message": {
        "id": "msg_01XFDUDYJgAACzvnptvVoYEL", "type": "message", "role": "assistant", "model": model,
        "content": [], "stop_reason": null, "stop_sequence": null, "usage": usage
    }})
}

fn delta_end(stop_reason: &str, usage: Value) -> Value {
    json!({"type": "message_delta", "delta": {"stop_reason": stop_reason, "stop_sequence": null}, "usage": usage})
}

fn stop() -> Value {
    json!({"type": "message_stop"})
}

// ------------------------------------------------------------------ requests

#[tokio::test]
async fn defaults_are_the_highest_the_model_offers() {
    // Claude 5.x and Opus 4.7/4.8: adaptive thinking, the top effort, summaries
    // instead of the hidden (`omitted`) default.
    for model in [
        "claude-fable-5-2",
        "claude-mythos-5-2",
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-sonnet-5-5",
        "claude-sonnet-5",
        "claude-haiku-5-5",
    ] {
        let p = prep_ok(model, &[]).await;
        assert_eq!(p.url, "https://api.anthropic.com/v1/messages");
        assert_eq!(p.dialect, DIALECT);
        assert_eq!(p.body["model"], model);
        assert_eq!(p.body["stream"], true);
        assert_eq!(p.body["thinking"], json!({"type": "adaptive", "display": "summarized"}), "{model}");
        assert_eq!(p.body["output_config"], json!({"effort": "max"}), "{model}");
        assert_eq!(p.body["max_tokens"], 128_000, "{model}");
        assert_eq!(p.auto_max_tokens, Some(128_000));
        assert_eq!(p.body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(p.body["messages"], json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}]));
        for absent in ["tools", "speed", "metadata", "system", "service_tier", "temperature", "container"] {
            assert!(p.body.get(absent).is_none(), "{model}: {absent}");
        }
        assert_eq!(p.settings, vec!["effort max"], "{model}");
        assert!(p.warnings.is_empty() && p.notes.is_empty(), "{model}");
        assert!(betas(model, &p.body).is_empty(), "{model}");
        assert!(!p.long_decode);
        assert_eq!(p.applied[0].patch, json!({"anthropic-version": "2026-10-21"}));
        let reasoning = p.applied.iter().find(|a| a.patch.get("thinking").is_some()).unwrap();
        assert_eq!(reasoning.source, "default");
    }
    // 4.6: `summarized` is already the default; no `xhigh`, so `max` is the top.
    for model in ["claude-opus-4-6", "claude-sonnet-4-6"] {
        let p = prep_ok(model, &[]).await;
        assert_eq!(p.body["thinking"], json!({"type": "adaptive"}), "{model}");
        assert_eq!(p.body["output_config"], json!({"effort": "max"}));
    }
    // 4.5 generation: manual thinking with the largest valid budget.
    let p = prep_ok("claude-opus-4-5", &[]).await;
    assert_eq!(p.body["max_tokens"], 64_000);
    assert_eq!(p.body["thinking"], json!({"type": "enabled", "budget_tokens": 63_999}));
    assert_eq!(p.body["output_config"], json!({"effort": "high"}));
    assert_eq!(p.settings, vec!["effort high", "thinking budget 63999"]);
    for model in ["claude-haiku-4-5", "claude-haiku-4-5-20251001"] {
        let p = prep_ok(model, &[]).await;
        assert_eq!(p.body["thinking"], json!({"type": "enabled", "budget_tokens": 63_999}), "{model}");
        assert!(p.body.get("output_config").is_none(), "{model}: no effort parameter");
        assert_eq!(p.settings, vec!["thinking budget 63999"]);
    }
}

#[tokio::test]
async fn end_user_id_goes_to_metadata() {
    let h = [Turn::user_text("hi")];
    let o = opts(&[]);
    let mut i = input("claude-opus-5-5", &o, &h);
    i.user = Some("8f14e45fceea167a5a36dedd4bea2543".into());
    let p = provider().prepare(&i, &[], &Blobs::default()).await.unwrap();
    assert_eq!(p.body["metadata"], json!({"user_id": "8f14e45fceea167a5a36dedd4bea2543"}));
    // The conversation key has no documented field on this API.
    assert!(!p.body.to_string().contains("tg-1"));
}

#[tokio::test]
async fn effort_and_thinking_options() {
    let p = prep_ok("claude-opus-5-5", &["e=low"]).await;
    assert_eq!(p.body["output_config"], json!({"effort": "low"}));
    assert_eq!(p.body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(p.settings, vec!["effort low"]);
    assert_eq!(p.applied.iter().find(|a| a.patch.get("thinking").is_some()).unwrap().source, "option");

    let e = prep_err("claude-opus-4-6", &["e=xhigh"]).await;
    assert!(e.contains("levels: max, high, medium, low"), "{e}");
    let e = prep_err("claude-opus-4-5", &["e=max"]).await;
    assert!(e.contains("levels: high, medium, low"), "{e}");
    let e = prep_err("claude-haiku-4-5", &["e=high"]).await;
    assert!(e.contains("has no reasoning effort levels"), "{e}");

    // Thinking off: impossible on always-thinking models.
    for model in ["claude-opus-5-5", "claude-fable-5-2", "claude-fable-5-1", "claude-mythos-5"] {
        let e = prep_err(model, &["e=none"]).await;
        assert!(e.contains("cannot turn reasoning off"), "{model}: {e}");
    }
    // Sonnet 5.5: its documented off setting, which takes no other field.
    let p = prep_ok("claude-sonnet-5-5", &["e=none"]).await;
    assert_eq!(p.body["thinking"], json!({"type": "between_tools"}));
    assert!(p.body.get("output_config").is_none(), "effort stays at the API default (high), as between_tools needs");
    assert_eq!(p.settings, vec!["thinking between_tools (no up-front thinking)"]);
    // `disabled` elsewhere; the effort is left at the API default (high or below, as Opus 5 and Haiku 5.5 need).
    for model in ["claude-haiku-5-5", "claude-opus-5", "claude-sonnet-5", "claude-opus-4-8", "claude-haiku-4-5"] {
        let p = prep_ok(model, &["e=none"]).await;
        assert_eq!(p.body["thinking"], json!({"type": "disabled"}), "{model}");
        assert!(p.body.get("output_config").is_none(), "{model}");
        assert_eq!(p.settings, vec!["thinking off"]);
    }
    // Opus 4.5: effort composes with the largest budget.
    let p = prep_ok("claude-opus-4-5", &["e=low"]).await;
    assert_eq!(p.body["thinking"], json!({"type": "enabled", "budget_tokens": 63_999}));
    assert_eq!(p.body["output_config"], json!({"effort": "low"}));
}

#[tokio::test]
async fn thinking_budget_option() {
    let p = prep_ok("claude-haiku-4-5", &["budget=20000"]).await;
    assert_eq!(p.body["thinking"], json!({"type": "enabled", "budget_tokens": 20_000}));
    assert_eq!(p.settings, vec!["thinking budget 20000"]);
    // Opus 4.5 keeps its (highest) effort alongside the budget.
    let p = prep_ok("claude-opus-4-5", &["budget=8000"]).await;
    assert_eq!(p.body["output_config"], json!({"effort": "high"}));
    // Deprecated but still accepted on 4.6.
    let p = prep_ok("claude-sonnet-4-6", &["budget=4096"]).await;
    assert_eq!(p.body["thinking"], json!({"type": "enabled", "budget_tokens": 4096}));
    assert!(prep_err("claude-haiku-4-5", &["budget=500"]).await.contains("at least 1024"));
    assert!(prep_err("claude-haiku-4-5", &["budget=64000"]).await.contains("below max_tokens (64000)"));
    for model in ["claude-opus-5-5", "claude-opus-4-8", "claude-sonnet-5-5", "claude-haiku-5-5"] {
        let e = prep_err(model, &["budget=4096"]).await;
        assert!(e.contains("has no reasoning token budget"), "{model}: {e}");
    }
}

#[tokio::test]
async fn options_the_api_does_not_have_are_errors() {
    let e = prep_err("claude-opus-5-5", &["m"]).await;
    assert_eq!(e, "`claude-opus-5-5` has no Google Maps (`+m`)");
    assert!(prep_err("claude-opus-5-5", &["v=high"]).await.contains("has no verbosity setting"));
    assert!(prep_err("claude-opus-5-5", &["4W"]).await.contains("has no image size/aspect options"));
    assert_eq!(
        prep_err("claude-opus-5-5", &["p=anthropic"]).await,
        "`p=` (provider choice) does not apply to Anthropic"
    );
    assert!(prep_err("claude-opus-5-5", &["tier=priority"]).await.contains("tiers: fast"));
    assert!(prep_err("claude-opus-5-5", &["tier=flex"]).await.contains("tiers: fast"));
    let e = prep_err("claude-sonnet-5-5", &["tier=fast"]).await;
    assert!(e.contains("has no service tiers"), "{e}");
    assert!(prep_err("claude-opus-5-5", &["temperature=0"]).await.contains("unknown option"));
}

#[tokio::test]
async fn fast_mode_and_tiers() {
    let p = prep_ok("claude-opus-5-5", &["tier=fast"]).await;
    assert_eq!(p.body["speed"], "fast");
    assert_eq!(p.settings, vec!["effort max", "tier fast"]);
    assert!(betas("claude-opus-5-5", &p.body).is_empty(), "GA on Opus 5.5");
    let p = prep_ok("claude-fable-5-2", &["tier=fast"]).await;
    assert!(betas("claude-fable-5-2", &p.body).is_empty());
    for model in ["claude-opus-5", "claude-opus-4-8"] {
        let p = prep_ok(model, &["tier=fast"]).await;
        assert_eq!(betas(model, &p.body), vec![FAST_MODE_BETA], "{model}");
        assert!(p.applied.iter().any(|a| a.patch == json!({"anthropic-beta": "fast-mode-2026-02-01"})));
    }
    // The platform defaults change nothing.
    for t in ["tier=auto", "tier=default"] {
        let p = prep_ok("claude-opus-5-5", &[t]).await;
        assert!(p.body.get("speed").is_none() && p.body.get("service_tier").is_none());
    }
}

#[tokio::test]
async fn server_tools_at_their_current_versions() {
    let p = prep_ok("claude-opus-5-5", &["s"]).await;
    assert_eq!(p.body["tools"], json!([{"type": "web_search_20260318", "name": "web_search"}]));
    assert_eq!(p.settings, vec!["effort max", "web search"]);
    assert!(p.notes.is_empty(), "code execution behind dynamic filtering is free");
    // A fixed order whatever the option order (the tool list is part of the replay prefix).
    let p = prep_ok("claude-opus-5-5", &["cus"]).await;
    assert_eq!(
        p.body["tools"],
        json!([
            {"type": "web_search_20260318", "name": "web_search"},
            {"type": "web_fetch_20260318", "name": "web_fetch", "citations": {"enabled": true}},
            {"type": "code_execution_20260521", "name": "code_execution"}
        ]),
        "web fetch cites the fetched pages (off by default)"
    );
    assert_eq!(p.settings, vec!["effort max", "web search", "URL fetching", "code execution"]);
    // Code execution alone is billed by container time, which the estimate leaves
    // out: a "+ code execution" marker after the cost, not a note.
    let p = prep_ok("claude-haiku-5-5", &["c"]).await;
    assert_eq!(p.body["tools"], json!([{"type": "code_execution_20260521", "name": "code_execution"}]));
    assert_eq!(p.settings, vec!["effort max", "code execution", "+ code execution"]);
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    // With dynamic-filtering web search or fetch it is free: no marker.
    let p = prep_ok("claude-haiku-5-5", &["cs"]).await;
    assert!(!p.settings.iter().any(|s| s == "+ code execution"), "{:?}", p.settings);
    // The basic web tools (4.5 generation) do not make it free.
    let p = prep_ok("claude-haiku-4-5", &["cs"]).await;
    assert_eq!(p.settings.last().map(String::as_str), Some("+ code execution"), "{:?}", p.settings);
    // Before Claude 4.6: the basic web tools, and interleaved thinking with a manual budget.
    let p = prep_ok("claude-opus-4-5", &["su"]).await;
    assert_eq!(
        p.body["tools"],
        json!([{"type": "web_search_20250305", "name": "web_search"},
               {"type": "web_fetch_20250910", "name": "web_fetch", "citations": {"enabled": true}}])
    );
    assert_eq!(betas("claude-opus-4-5", &p.body), vec![INTERLEAVED_THINKING_BETA]);
    assert_eq!(
        p.settings,
        vec!["effort high", "thinking budget 63999", "interleaved thinking", "web search", "URL fetching"]
    );
    let p = prep_ok("claude-opus-4-5", &["s", "e=none"]).await;
    assert!(betas("claude-opus-4-5", &p.body).is_empty(), "no manual thinking, no interleaving header");
    let p = prep_ok("claude-haiku-4-5", &["s"]).await;
    assert!(betas("claude-haiku-4-5", &p.body).is_empty(), "Haiku 4.5 has no interleaved thinking");
    // Adaptive thinking interleaves on its own.
    assert!(betas("claude-sonnet-4-6", &prep_ok("claude-sonnet-4-6", &["s"]).await.body).is_empty());
    assert_eq!(
        betas("claude-sonnet-4-6", &prep_ok("claude-sonnet-4-6", &["s", "budget=9000"]).await.body),
        vec![INTERLEAVED_THINKING_BETA]
    );
}

#[tokio::test]
async fn unknown_models_are_not_supported() {
    // Its price is unknown, so the bot does not use it, whatever the options.
    for o in [&[][..], &["e=xhigh", "s", "tier=fast"]] {
        assert_eq!(
            prep_err("claude-opus-6", o).await,
            "`claude-opus-6` is not in the bot's Anthropic model table (its price is unknown), so it is not supported"
        );
    }
    assert!(!provider().knows("claude-opus-6") && provider().knows("claude-opus-5-5"));
}

#[tokio::test]
async fn output_budget_follows_context_cap_and_overrides() {
    // The configured cap.
    let h = [Turn::user_text("hi")];
    let o = opts(&[]);
    let p = provider_at("https://api.anthropic.com/v1", Some(50_000))
        .prepare(&input("claude-opus-5-5", &o, &h), &[], &Blobs::default())
        .await
        .unwrap();
    assert_eq!(p.body["max_tokens"], 50_000);
    // After a context overflow: the bot's smaller value, and the budget below it.
    let mut i = input("claude-haiku-4-5", &o, &h);
    i.max_tokens_override = Some(5000);
    let p = provider().prepare(&i, &[], &Blobs::default()).await.unwrap();
    assert_eq!(p.body["max_tokens"], 5000);
    assert_eq!(p.body["thinking"]["budget_tokens"], 4999);
    assert_eq!(p.auto_max_tokens, Some(5000));
    // A long conversation on a 200K model leaves less room for output.
    let long = [Turn::user_text("a".repeat(480_000))];
    let p = prep("claude-haiku-4-5", &[], &long).await.unwrap();
    let m = p.body["max_tokens"].as_u64().unwrap();
    assert!(m < 64_000 && m > 30_000, "{m}");
    assert_eq!(p.body["thinking"]["budget_tokens"], m - 1);
    // No room for the minimum budget: say so instead of sending a request that cannot work.
    let mut i = input("claude-haiku-4-5", &o, &h);
    i.max_tokens_override = Some(1024);
    let e = provider().prepare(&i, &[], &Blobs::default()).await.unwrap_err();
    assert!(e.message.contains("no room for a thinking budget"), "{e}");
}

// ------------------------------------------------------------------ history

/// What a web-search answer from Opus 5.5 looks like in the stored payload
/// (the docs' example: empty thinking under `display: omitted` with its
/// signature, a search and its encrypted results, cited text).
fn searched_payload(tools: Value) -> Value {
    json!({"role": "assistant", "tools": tools, "content": [
        {"type": "thinking", "thinking": "", "signature": "EosnCkYICxIMMb3LzNrMu"},
        {"type": "redacted_thinking", "data": "EmwKAhgBEgy3va3pzix/LafPsn4a"},
        {"type": "server_tool_use", "id": "srvtoolu_01WYG3ziw53XMcoyKL4XcZmE", "name": "web_search", "input": {"query": "claude shannon birth date"}},
        {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_01WYG3ziw53XMcoyKL4XcZmE", "content": [
            {"type": "web_search_result", "url": "https://en.wikipedia.org/wiki/Claude_Shannon", "title": "Claude Shannon - Wikipedia",
             "encrypted_content": "EqgfCioIARgBIiQ3YTAwMjY1Mi1mZjM5", "page_age": "April 30, 2025"}]},
        {"type": "text", "text": "Claude Shannon was born on April 30, 1916", "citations": [
            {"type": "web_search_result_location", "url": "https://en.wikipedia.org/wiki/Claude_Shannon",
             "title": "Claude Shannon - Wikipedia", "encrypted_index": "Eo8BCioIAhgBIiQyYjQ0OWJmZi1lNm", "cited_text": "Claude Elwood Shannon (April 30, 1916 –"}]}
    ]})
}

#[tokio::test]
async fn native_turns_replay_verbatim_with_the_same_model_and_tools() {
    let search = json!([{"type": "web_search_20260318", "name": "web_search"}]);
    let payload = searched_payload(search.clone());
    let history = vec![
        system("Answer briefly."),
        Turn::user_text("When was Claude Shannon born?"),
        assistant("claude-opus-5-5", payload.clone(), "Claude Shannon was born on April 30, 1916"),
        Turn::user_text("And where?"),
    ];
    let p = prep("claude-opus-5-5", &["s"], &history).await.unwrap();
    assert_eq!(p.body["system"], json!([{"type": "text", "text": "Answer briefly."}]));
    let msgs = p.body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[1], json!({"role": "assistant", "content": payload["content"]}), "every block as received");
    // A cache breakpoint where the previous request's prompt ended; the newest
    // turn is left to automatic caching.
    assert_eq!(msgs[0]["content"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert!(msgs[2]["content"][0].get("cache_control").is_none());
    assert!(p.applied.iter().any(|a| a.patch.get("messages[0].content[0].cache_control").is_some()));
    assert!(p.notes.is_empty());

    // Without web search now: the tool list changed, so the answer goes back as text.
    let p = prep("claude-opus-5-5", &[], &history).await.unwrap();
    assert_eq!(
        p.body["messages"][1],
        json!({"role": "assistant", "content": [{"type": "text", "text": "Claude Shannon was born on April 30, 1916"}]})
    );
    assert!(p.notes[0].contains("other tools"), "{:?}", p.notes);
    // Another model: text only, no note (that is what switching models means).
    let p = prep("claude-sonnet-5-5", &["s"], &history).await.unwrap();
    assert_eq!(
        p.body["messages"][1]["content"],
        json!([{"type": "text", "text": "Claude Shannon was born on April 30, 1916"}])
    );
    assert!(p.notes.is_empty());
    // Same model and tools from OpenRouter's Messages skin: another dialect, text only.
    let mut or = history.clone();
    or[2].native.as_mut().unwrap().dialect = "openrouter.messages".into();
    let p = prep("claude-opus-5-5", &["s"], &or).await.unwrap();
    assert_eq!(p.body["messages"][1]["content"][0]["type"], "text");
    // A payload whose blocks were replaced by the answer text (as omni-bot's
    // long-decode merge does today) can only go back as text.
    let mut merged = history.clone();
    merged[2].native.as_mut().unwrap().payload["content"] = json!("Claude Shannon was born on April 30, 1916");
    let p = prep("claude-opus-5-5", &["s"], &merged).await.unwrap();
    assert_eq!(
        p.body["messages"][1]["content"],
        json!([{"type": "text", "text": "Claude Shannon was born on April 30, 1916"}])
    );
}

#[tokio::test]
async fn a_paused_and_continued_turn_replays_as_its_requests() {
    let payload = json!({"role": "assistant", "hops": [2, 1], "content": [
        {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {"query": "a"}},
        {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_1", "content": []},
        {"type": "text", "text": "done"}
    ]});
    let history = vec![Turn::user_text("q"), assistant("claude-haiku-5-5", payload, "done"), Turn::user_text("more")];
    let p = prep("claude-haiku-5-5", &[], &history).await.unwrap();
    let msgs = p.body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2);
    assert_eq!(msgs[2], json!({"role": "assistant", "content": [{"type": "text", "text": "done"}]}));
}

#[tokio::test]
async fn system_turns_and_answers_without_text() {
    // A system turn after the start stays where it is, on models that take it.
    let history = vec![Turn::user_text("hi"), system("Be terse.")];
    let p = prep("claude-opus-5-5", &[], &history).await.unwrap();
    assert!(p.body.get("system").is_none());
    assert_eq!(p.body["messages"][1], json!({"role": "system", "content": [{"type": "text", "text": "Be terse."}]}));
    let e = prep("claude-sonnet-5", &[], &history).await.unwrap_err();
    assert!(e.message.contains("only at the start"), "{e}");
    // An answer whose only output was thinking cannot be given to another model.
    let thought = assistant(
        "claude-opus-5-5",
        json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "", "signature": "x"}]}),
        "",
    );
    let history = vec![Turn::user_text("q"), thought, Turn::user_text("go on")];
    let e = prep("claude-sonnet-5-5", &[], &history).await.unwrap_err();
    assert!(e.message.contains("no text to give"), "{e}");
    // The same model takes it back as it was.
    let p = prep("claude-opus-5-5", &[], &history).await.unwrap();
    assert_eq!(p.body["messages"][1]["content"][0]["signature"], "x");
}

#[tokio::test]
async fn user_media_is_sent_natively() {
    let mut blobs = Blobs::default();
    let jpg = blobs.add(b"\xff\xd8\xff jpeg", "image/jpeg");
    let jpg_label = blobs.add(b"\xff\xd8\xff other", "image/jpg");
    let pdf = blobs.add(b"%PDF-1.7 doc", "application/pdf");
    let txt = blobs.add("第一行\nline 2".as_bytes(), "text/plain");
    let code = blobs.add(b"fn main() {}", "application/octet-stream");
    let turn = Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![
            Part::Image { blob: jpg.clone(), filename: None, generated: false, dims: Some((10, 10)), thought: false },
            Part::File { blob: jpg_label, filename: Some("scan.jpg".into()), generated: false },
            Part::File { blob: pdf, filename: Some("paper.pdf".into()), generated: false },
            Part::File { blob: txt, filename: Some("notes.txt".into()), generated: false },
            Part::File { blob: code, filename: Some("main.rs".into()), generated: false },
            Part::text("Compare them."),
        ],
        native: None,
    };
    let o = opts(&[]);
    let h = [turn];
    let p = provider().prepare(&input("claude-opus-5-5", &o, &h), &[], &blobs).await.unwrap();
    let c = &p.body["messages"][0]["content"];
    let b64 = crate::provider::b64;
    assert_eq!(
        c[0],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64(b"\xff\xd8\xff jpeg")}})
    );
    assert_eq!(c[1]["source"]["media_type"], "image/jpeg");
    // Every document can be cited (all or none, from the first turn on).
    assert_eq!(
        c[2],
        json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": b64(b"%PDF-1.7 doc")},
               "title": "paper.pdf", "citations": {"enabled": true}})
    );
    assert_eq!(
        c[3],
        json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "第一行\nline 2"},
               "title": "notes.txt", "citations": {"enabled": true}})
    );
    assert_eq!(c[4]["source"]["data"], "fn main() {}");
    assert_eq!(c[4]["citations"], json!({"enabled": true}));
    assert_eq!(c[5], json!({"type": "text", "text": "Compare them."}));
    assert!(p.notes.is_empty(), "text files are native documents, nothing is inlined");
    assert!(c[0].get("citations").is_none() && c[1].get("citations").is_none(), "images are not documents");
    assert!(p.applied.iter().any(|a| a.patch.get("messages[].content[document].citations").is_some()));
    assert!(super::stream::citations_on(&p.body));

    let one = |part: Part| vec![Turn { role: Role::User, origin: Origin::Live, parts: vec![part], native: None }];
    let mut blobs = Blobs::default();
    let cases = [
        (
            Part::Image {
                blob: blobs.add(b"heic", "image/heic"),
                filename: None,
                generated: false,
                dims: None,
                thought: false,
            },
            "image/heic images",
        ),
        (
            Part::File {
                blob: blobs.add(b"\xff\xfe", "text/plain"),
                filename: Some("bad.txt".into()),
                generated: false,
            },
            "not UTF-8",
        ),
        (
            Part::File { blob: blobs.add(b"ID3", "audio/mpeg"), filename: Some("a.mp3".into()), generated: false },
            "audio/mpeg files",
        ),
        (
            Part::File {
                blob: blobs.add(b"PK", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
                filename: Some("a.docx".into()),
                generated: false,
            },
            "officedocument",
        ),
        (
            Part::RemoteMedia { url: "https://youtu.be/x".into(), kind: omni_core::content::RemoteMediaKind::YouTube },
            "remote media",
        ),
        (
            Part::Location { latitude: 1.0, longitude: 2.0, accuracy_m: None, title: None, address: None },
            "does not take a location via Anthropic",
        ),
    ];
    for (part, needle) in cases {
        let h = one(part);
        let e = provider().prepare(&input("claude-opus-5-5", &o, &h), &[], &blobs).await.unwrap_err();
        assert!(e.message.contains(needle), "{needle}: {e}");
        assert!(e.message.contains("claude-opus-5-5"), "{e}");
    }
}

#[tokio::test]
async fn generated_media_of_other_models_is_an_error() {
    let mut blobs = Blobs::default();
    let png = blobs.add(b"png", "image/png");
    let draft = blobs.add(b"draft", "image/png");
    let mp4 = blobs.add(b"....ftypmp42", "video/mp4");
    let answer = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![
            Part::Image { blob: draft, filename: None, generated: true, dims: None, thought: true },
            Part::text("Here it is."),
            Part::Image { blob: png, filename: None, generated: true, dims: None, thought: false },
        ],
        native: None,
    };
    let history = vec![Turn::user_text("draw"), answer.clone(), Turn::user_text("describe it")];
    let e = prep("claude-opus-5-5", &[], &history).await.unwrap_err();
    assert_eq!(
        e.message,
        "1 image generated in an earlier turn cannot be sent: Claude sees only the text of other models' answers; send it the file as an attachment",
        "the draft made while thinking is reasoning, not counted"
    );
    assert_eq!(e.retry, Retry::Never);
    // A draft alone is reasoning, which never goes to another model: the text only,
    // nothing moved into a user message.
    let drafted = Turn { parts: answer.parts[..2].to_vec(), ..answer.clone() };
    let p = prep("claude-opus-5-5", &[], &[Turn::user_text("draw"), drafted, Turn::user_text("describe it")])
        .await
        .unwrap();
    assert_eq!(p.body["messages"][1]["content"], json!([{"type": "text", "text": "Here it is."}]));
    assert_eq!(p.body["messages"].as_array().unwrap().len(), 3);
    assert!(p.warnings.is_empty() && p.notes.is_empty(), "{:?} {:?}", p.warnings, p.notes);
    // Two answers with media.
    let mut video = answer.clone();
    video.parts.push(Part::File { blob: mp4, filename: Some("clip.mp4".into()), generated: true });
    let history = vec![Turn::user_text("draw"), answer, Turn::user_text("film it"), video, Turn::user_text("more")];
    let e = prep("claude-opus-5-5", &[], &history).await.unwrap_err();
    assert_eq!(
        e.message,
        "2 images and 1 video generated in earlier turns cannot be sent: Claude sees only the text of other models' answers; send it the file as an attachment"
    );
    // An answer that is only an image has nothing Claude can take.
    let only = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::Image {
            blob: blobs.add(b"x", "image/png"),
            filename: None,
            generated: true,
            dims: None,
            thought: false,
        }],
        native: None,
    };
    let e = prep("claude-opus-5-5", &[], &[Turn::user_text("draw"), only, Turn::user_text("?")]).await.unwrap_err();
    assert!(e.message.contains("no text to give") && e.message.contains("1 image generated"), "{e}");
}

// ------------------------------------------------------------------ stream

fn spec(model: &str) -> Option<&'static models::ModelSpec> {
    models::model_spec(model)
}

#[test]
fn summarized_thinking_and_text() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    let evs = feed(
        &mut p,
        &[
            message_start(
                "claude-opus-5-5",
                json!({"input_tokens": 1200, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 800, "output_tokens": 1}),
            ),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "The user asks "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "for the GCD."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3h"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "The GCD is 21."}}),
            json!({"type": "content_block_stop", "index": 1}),
            delta_end("end_turn", json!({"output_tokens": 300, "output_tokens_details": {"thinking_tokens": 250}})),
            stop(),
        ],
    );
    assert!(crate::chatstream::EventParser::finished(&p) && !p.paused());
    // The message id is not the upstream id (the request-id header is).
    assert_eq!(evs[0], StreamEvent::Meta { upstream_id: None, provider: None, model: Some("claude-opus-5-5".into()) });
    let StreamEvent::LiveUsage(live) = &evs[1] else { panic!("{:?}", evs[1]) };
    assert_eq!((live.input_tokens, live.output_tokens, live.cost_usd), (Some(2000), None, None));
    let acc = accumulate(&evs);
    let parts = acc.parts();
    assert!(
        matches!(&parts[0], Part::Reasoning(r) if r.kind == ReasoningKind::Summary && r.text.as_deref() == Some("The user asks for the GCD."))
    );
    assert_eq!(parts[1], Part::text("The GCD is 21."));
    assert_eq!(acc.finish_reason.as_deref(), Some("stop"));
    assert_eq!(acc.native_finish_reason.as_deref(), Some("end_turn"));
    let u = acc.usage.unwrap();
    assert_eq!(
        (u.input_tokens, u.cached_input_tokens, u.output_tokens, u.reasoning_tokens),
        (Some(2000), Some(800), Some(300), Some(250))
    );
    // 1200 × $4 + 800 × $0.20 + 300 × $20, per million.
    assert!(close(u.cost_usd.unwrap(), (1200.0 * 4.0 + 800.0 * 0.20 + 300.0 * 20.0) / 1e6));
    assert_eq!(u.cost_source, CostSource::Estimated);
    // The message id is kept for the next turn's cache diagnostics.
    assert_eq!(p.message_id(), Some("msg_01XFDUDYJgAACzvnptvVoYEL"));
    let native = p.native_payload(None);
    assert_eq!(
        native,
        json!({"role": "assistant", "id": "msg_01XFDUDYJgAACzvnptvVoYEL", "content": [
            {"type": "thinking", "thinking": "The user asks for the GCD.", "signature": "EqQBCgIYAhIM1gbcDa9GJwZA2b3h"},
            {"type": "text", "text": "The GCD is 21."}
        ]})
    );
    assert!(p.refusal().is_none());
}

/// The docs' trace under `display: "omitted"`: the block is kept (empty
/// thinking, signature) for replay, nothing is shown.
#[test]
fn omitted_thinking_is_kept_but_not_shown() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 10, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "EosnCkYICxIMMb3LzNrMu"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "EmwKAhgB"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "12,231"}}),
            json!({"type": "content_block_stop", "index": 2}),
            delta_end("end_turn", json!({"output_tokens": 40})),
            stop(),
        ],
    );
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Reasoning { .. })));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::OpaqueReasoning { kind: ReasoningKind::Redacted })));
    let native = p.native_payload(None);
    assert_eq!(native["content"][0], json!({"type": "thinking", "thinking": "", "signature": "EosnCkYICxIMMb3LzNrMu"}));
    assert_eq!(native["content"][1], json!({"type": "redacted_thinking", "data": "EmwKAhgB"}));
}

/// The docs' web search stream (Streaming messages): a server tool call whose
/// input streams, a result block that arrives whole, cited text.
#[test]
fn web_search_stream() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    let evs = feed(
        &mut p,
        &[
            message_start(
                "claude-opus-5-5",
                json!({"input_tokens": 2679, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 3}),
            ),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "I'll check the current weather in New York City for you."}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"query"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "\": \"weather NYC today\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_014hJH82Qum7Td6UV8gDXThB", "content": [
                {"type": "web_search_result", "title": "Weather in New York City", "url": "https://world-weather.info/forecast/usa/new_york/", "encrypted_content": "Ev0DCioIAxgCIiQ3NmU4ZmI4OC1k", "page_age": null}
            ]}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "It is 18°C and sunny."}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "citations_delta", "citation": {
            "type": "web_search_result_location", "url": "https://world-weather.info/forecast/usa/new_york/",
            "title": "Weather in New York City", "encrypted_index": "Eo8BCioIAhgBIiQyYjQ0", "cited_text": "18°C, sunny"}}}),
            json!({"type": "content_block_stop", "index": 3}),
            delta_end(
                "end_turn",
                json!({"input_tokens": 10682, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0, "output_tokens": 510, "server_tool_use": {"web_search_requests": 1}}),
            ),
            stop(),
        ],
    );
    let tools: Vec<&ToolCall> =
        evs.iter().filter_map(|e| if let StreamEvent::Tool(t) = e { Some(t) } else { None }).collect();
    assert_eq!(tools.len(), 3);
    assert_eq!((tools[0].status.as_deref(), tools[0].input.as_ref()), (Some("in_progress"), None));
    assert_eq!(tools[1].input, Some(json!({"query": "weather NYC today"})));
    assert_eq!(tools[2].status.as_deref(), Some("completed"));
    assert_eq!(
        tools[2].output,
        Some(
            json!([{"type": "web_search_result", "title": "Weather in New York City", "url": "https://world-weather.info/forecast/usa/new_york/", "page_age": null}])
        ),
        "results without the encrypted content"
    );
    let acc = accumulate(&evs);
    let parts = acc.parts();
    assert!(matches!(&parts[0], Part::Text { text, .. } if text.starts_with("I'll check")));
    assert!(
        matches!(&parts[1], Part::ToolCall(t) if t.name == "web_search" && t.status.as_deref() == Some("completed") && t.input.is_some() && t.output.is_some())
    );
    let Part::Text { text, citations } = &parts[2] else { panic!("{:?}", parts[2]) };
    assert_eq!(text, "It is 18°C and sunny.");
    assert_eq!(
        citations[0],
        Citation {
            url: Some("https://world-weather.info/forecast/usa/new_york/".into()),
            title: Some("Weather in New York City".into()),
            cited_text: Some("18°C, sunny".into()),
            ..Default::default()
        }
    );
    // Tokens plus $10 per 1,000 searches.
    let u = acc.usage.unwrap();
    assert!(close(u.cost_usd.unwrap(), (10682.0 * 4.0 + 510.0 * 20.0) / 1e6 + 0.01));
    // Replay needs every block exactly as received, encrypted content included.
    let native = p.native_payload(Some(&json!([{"type": "web_search_20260318", "name": "web_search"}])));
    assert_eq!(native["content"][1]["input"], json!({"query": "weather NYC today"}));
    assert_eq!(native["content"][2]["content"][0]["encrypted_content"], "Ev0DCioIAxgCIiQ3NmU4ZmI4OC1k");
    assert_eq!(native["content"][3]["citations"][0]["encrypted_index"], "Eo8BCioIAhgBIiQyYjQ0");
    assert_eq!(native["tools"][0]["type"], "web_search_20260318");
}

/// A tool input whose chunks are all empty stays the block start's `{}`:
/// replay needs an object, never the unparsable "".
#[test]
fn empty_tool_input_chunks_keep_an_object() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 10, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_e", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": ""}}),
            json!({"type": "content_block_stop", "index": 0}),
        ],
    );
    assert_eq!(p.content()[0]["input"], json!({}));
    assert!(evs.iter().all(|e| !matches!(e, StreamEvent::Tool(t) if t.input == Some(json!("")))));
}

#[test]
fn code_execution_and_fetch_results() {
    let mut p = Parser::new(spec("claude-sonnet-5-5"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-sonnet-5-5", json!({"input_tokens": 100, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_b", "name": "bash_code_execution"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"python analyze.py\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_b", "content": {
            "type": "bash_code_execution_result", "stdout": "   A  B\n0  1  2", "stderr": "", "return_code": 0,
            "content": [{"type": "bash_code_execution_output", "file_id": "file_011CNha8iCJcU1wXNR6q4V8w"}]}}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "server_tool_use", "id": "srvtoolu_t", "name": "text_editor_code_execution", "input": {"command": "view", "path": "config.json"}}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text_editor_code_execution_tool_result", "tool_use_id": "srvtoolu_t", "content": {
            "type": "text_editor_code_execution_tool_result_error", "error_code": "file_not_found"}}}),
            json!({"type": "content_block_stop", "index": 3}),
            // Dynamic filtering: code that called web search returns its stdout encrypted.
            json!({"type": "content_block_start", "index": 4, "content_block": {"type": "server_tool_use", "id": "srvtoolu_c", "name": "code_execution", "input": {"code": "r = await web_search({'query': 'x'})"}}}),
            json!({"type": "content_block_stop", "index": 4}),
            json!({"type": "content_block_start", "index": 5, "content_block": {"type": "code_execution_tool_result", "tool_use_id": "srvtoolu_c", "content": {
            "type": "encrypted_code_execution_result", "encrypted_stdout": "EuQJCioIDhgC", "stderr": "", "return_code": 0, "content": []}}}),
            json!({"type": "content_block_stop", "index": 5}),
            json!({"type": "content_block_start", "index": 6, "content_block": {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f", "content": {
            "type": "web_fetch_result", "url": "https://example.com/article", "retrieved_at": "2025-08-25T10:30:00Z",
            "content": {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "Full text content of the article..."}, "title": "Article Title"}}}}),
            json!({"type": "content_block_stop", "index": 6}),
            delta_end("end_turn", json!({"output_tokens": 50})),
            stop(),
        ],
    );
    let tools: Vec<&ToolCall> =
        evs.iter().filter_map(|e| if let StreamEvent::Tool(t) = e { Some(t) } else { None }).collect();
    let by = |id: &str, status: &str| {
        tools.iter().find(|t| t.id.as_deref() == Some(id) && t.status.as_deref() == Some(status)).copied()
    };
    assert_eq!(by("srvtoolu_b", "in_progress").unwrap().input, None);
    assert_eq!(tools[1].input, Some(json!({"command": "python analyze.py"})));
    assert_eq!(
        by("srvtoolu_b", "completed").unwrap().output,
        Some(
            json!({"type": "bash_code_execution_result", "stdout": "   A  B\n0  1  2", "stderr": "", "return_code": 0,
            "content": [{"type": "bash_code_execution_output", "file_id": "file_011CNha8iCJcU1wXNR6q4V8w"}]})
        )
    );
    // An input that arrives whole in the block start is shown too.
    assert_eq!(by("srvtoolu_t", "in_progress").unwrap().input, Some(json!({"command": "view", "path": "config.json"})));
    let failed = by("srvtoolu_t", "failed").unwrap();
    assert_eq!(
        failed.output,
        Some(json!({"type": "text_editor_code_execution_tool_result_error", "error_code": "file_not_found"}))
    );
    assert_eq!(failed.name, "text_editor_code_execution");
    assert_eq!(
        by("srvtoolu_c", "completed").unwrap().output,
        Some(json!({"type": "encrypted_code_execution_result", "stderr": "", "return_code": 0, "content": []}))
    );
    // The fetch result as sent, without the page's body (only its length).
    assert_eq!(
        by("srvtoolu_f", "completed").unwrap().output,
        Some(
            json!({"type": "web_fetch_result", "url": "https://example.com/article", "retrieved_at": "2025-08-25T10:30:00Z",
            "content": {"type": "document", "title": "Article Title", "source": {"type": "text", "media_type": "text/plain",
            "data": "(35 characters, sent back to the model in later turns; not shown)"}}})
        )
    );
    let acc = accumulate(&evs);
    let calls: Vec<&ToolCall> =
        acc.parts().iter().filter_map(|p| if let Part::ToolCall(t) = p { Some(t) } else { None }).collect();
    assert_eq!(calls.len(), 4, "a call and its result are one part");
    // The fetched document stays in the payload for replay.
    assert_eq!(
        p.native_payload(None)["content"][6]["content"]["content"]["source"]["data"],
        "Full text content of the article..."
    );
}

#[test]
fn refusals_are_content_filter_stops_with_details() {
    // Before any output, cyber: not billed.
    let mut p = Parser::new(spec("claude-fable-5-2"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-fable-5-2", json!({"input_tokens": 412, "output_tokens": 0})),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_sequence": null, "stop_details": {
            "type": "refusal", "category": "cyber", "explanation": "This request was declined because it could enable cyber harm."}},
            "usage": {"output_tokens": 0}}),
            stop(),
        ],
    );
    let acc = accumulate(&evs);
    assert_eq!(
        (acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()),
        (Some("content_filter"), Some("refusal"))
    );
    assert_eq!(acc.finish_details.as_ref().unwrap()["category"], "cyber");
    let u = acc.usage.unwrap();
    assert_eq!((u.input_tokens, u.cost_usd, u.cost_source), (Some(412), Some(0.0), CostSource::Estimated));
    // `bio` is billed even before any output.
    let mut p = Parser::new(spec("claude-fable-5-2"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-fable-5-2", json!({"input_tokens": 1000, "output_tokens": 0})),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "bio", "explanation": null}}, "usage": {"output_tokens": 0}}),
            stop(),
        ],
    );
    assert!(close(accumulate(&evs).usage.unwrap().cost_usd.unwrap(), 1000.0 * 8.0 / 1e6));
    // Mid-stream: input and the streamed output are billed.
    let mut p = Parser::new(spec("claude-fable-5-2"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-fable-5-2", json!({"input_tokens": 1000, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Sure, first"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "general_harms", "explanation": "x"}}, "usage": {"output_tokens": 30}}),
            stop(),
        ],
    );
    let acc = accumulate(&evs);
    assert_eq!(acc.finish_reason.as_deref(), Some("content_filter"));
    assert!(close(acc.usage.unwrap().cost_usd.unwrap(), (1000.0 * 8.0 + 30.0 * 40.0) / 1e6));
}

/// Recorded 2026-11-11 (Fable 5.2 through OpenRouter's Messages skin, which
/// passes Anthropic's events through): a cyber refusal before any output.
#[test]
fn recorded_refusal_is_free() {
    let mut p = Parser::new(spec("claude-fable-5-2"));
    let evs = feed(&mut p, &crate::fixtures::sse("messages_refusal_fable52.sse"));
    let acc = accumulate(&evs);
    assert_eq!(acc.finish_reason.as_deref(), Some("content_filter"));
    assert!(acc.finish_details.unwrap()["explanation"].as_str().unwrap().contains("Usage Policy"));
    let u = acc.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens, u.cost_usd), (Some(25), Some(0), Some(0.0)));
}

#[test]
fn stop_reasons() {
    for (native, normalized) in [
        ("end_turn", "stop"),
        ("stop_sequence", "stop"),
        ("max_tokens", "length"),
        ("model_context_window_exceeded", "length"),
    ] {
        let mut p = Parser::new(spec("claude-haiku-5-5"));
        let evs = feed(
            &mut p,
            &[
                message_start("claude-haiku-5-5", json!({"input_tokens": 5, "output_tokens": 1})),
                delta_end(native, json!({"output_tokens": 7})),
                stop(),
            ],
        );
        let acc = accumulate(&evs);
        assert_eq!(
            (acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()),
            (Some(normalized), Some(native))
        );
        assert!(acc.finish_details.is_none());
    }
    // A recorded max_tokens stop (through OpenRouter, Anthropic's events).
    let c = crate::fixtures::case("msg_length");
    let mut p = Parser::new(spec("claude-haiku-5-5"));
    let evs = feed(&mut p, c["sse"].as_array().unwrap());
    let acc = accumulate(&evs);
    assert_eq!(acc.finish_reason.as_deref(), Some("length"));
    assert_eq!(acc.parts(), &[Part::text("1 2 3 4 5 6 7 8 ")]);
}

#[test]
fn in_band_errors_are_classified() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 5, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Partial"}}),
        ],
    );
    let ev = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    let e = p.event(Some("error"), &ev).unwrap_err();
    assert_eq!((e.kind, e.retry, e.status), (ErrorKind::InBand, Retry::Backoff, None));
    assert_eq!(e.error_type.as_deref(), Some("overloaded_error"));
    assert_eq!(e.message, "Overloaded");
    assert_eq!(e.details, Some(ev));
    assert!(e.partial_output);
    let ev = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "bad"}});
    assert_eq!(Parser::new(None).event(None, &ev).unwrap_err().retry, Retry::Never);
}

#[test]
fn a_paused_turn_continues_in_the_same_parser() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    let first = feed(
        &mut p,
        &[
            message_start(
                "claude-opus-5-5",
                json!({"input_tokens": 1000, "output_tokens": 1, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}),
            ),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Search first."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "s1"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":\"q\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "pause_turn", "stop_sequence": null,
            "container": {"id": "container_011CZ", "expires_at": "2026-11-12T08:00:00Z"}},
            "usage": {"output_tokens": 100, "server_tool_use": {"web_search_requests": 1}}}),
            stop(),
        ],
    );
    assert!(p.paused());
    assert_eq!(p.container(), Some("container_011CZ"));
    assert!(!first.iter().any(|e| matches!(e, StreamEvent::Finish { .. })), "pause_turn is not the end of the turn");
    assert_eq!(p.content().len(), 2);
    p.next_request();
    let second = feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 1500, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Found it."}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "s2"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Answer."}}),
            json!({"type": "content_block_stop", "index": 1}),
            delta_end("end_turn", json!({"output_tokens": 50})),
            stop(),
        ],
    );
    assert!(!p.paused());
    let StreamEvent::LiveUsage(live) = &second[1] else { panic!("{:?}", second[1]) };
    assert_eq!((live.input_tokens, live.output_tokens), (Some(2500), Some(100)), "both requests so far");
    let mut all = first.clone();
    all.extend(second);
    let acc = accumulate(&all);
    // The two thinking blocks stay two parts (block indices are offset).
    let reasoning: Vec<_> = acc.parts().iter().filter(|p| matches!(p, Part::Reasoning(_))).collect();
    assert_eq!(reasoning.len(), 2);
    let u = acc.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (Some(2500), Some(150)));
    assert!(close(
        u.cost_usd.unwrap(),
        (1000.0 * 4.0 + 100.0 * 20.0) / 1e6 + 0.01 + (1500.0 * 4.0 + 50.0 * 20.0) / 1e6
    ));
    assert_eq!(u.raw.as_array().unwrap().len(), 2, "both usage objects, in order");
    assert_eq!(acc.native_finish_reason.as_deref(), Some("end_turn"));
    let native = p.native_payload(None);
    assert_eq!(native["hops"], json!([2, 2]));
    assert_eq!(native["content"].as_array().unwrap().len(), 4);
    assert_eq!(native["content"][1]["input"], json!({"query": "q"}));
    assert_eq!(native["content"][2]["signature"], "s2");
}

// ------------------------------------------------------------------ errors

fn http(status: u16, retry_after: Option<u64>, body: &str) -> LlmError {
    http_error(status, retry_after, body, error_from_json)
}

#[test]
fn http_errors_are_classified() {
    // Recorded 2026-11-12 from the API without a key.
    let body = r#"{"type":"error","error":{"type":"authentication_error","message":"x-api-key header is required"},"request_id":"req_011CgxXdube14Bw3iW6aszXK"}"#;
    let e = http(401, None, body);
    assert_eq!((e.kind, e.retry, e.status), (ErrorKind::Http, Retry::Never, Some(401)));
    assert_eq!(e.error_type.as_deref(), Some("authentication_error"));
    assert_eq!(e.message, "x-api-key header is required");
    assert_eq!(e.details.unwrap(), serde_json::from_str::<Value>(body).unwrap(), "the whole body, request_id included");

    let err = |ty: &str, msg: &str| {
        format!(r#"{{"type":"error","error":{{"type":"{ty}","message":"{msg}"}},"request_id":"req_1"}}"#)
    };
    assert_eq!(
        http(
            429,
            Some(7),
            &err("rate_limit_error", "Number of request tokens has exceeded your per-minute rate limit")
        )
        .retry,
        Retry::After(7)
    );
    assert_eq!(http(429, None, &err("rate_limit_error", "rate limited")).retry, Retry::After(20));
    // The tier spend cap: a 429 that fails until next month.
    let cap = r#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your API usage limits: your organization has crossed its monthly API usage threshold, set based on your organization's API tier. You will regain access on 2026-12-01 at 00:00 UTC.","details":{"error_code":"enforced_spend_limit_reached"}},"request_id":"req_018EeWyXxfu5pfWkrYcMdjWG"}"#;
    let e = http(429, None, cap);
    assert_eq!(e.retry, Retry::Never);
    assert_eq!(e.error_type.as_deref(), Some("enforced_spend_limit_reached"));
    assert_eq!(http(529, None, &err("overloaded_error", "Overloaded")).retry, Retry::Backoff);
    assert_eq!(http(529, Some(30), &err("overloaded_error", "Overloaded")).retry, Retry::After(30));
    assert_eq!(http(500, None, &err("api_error", "Internal server error")).retry, Retry::Backoff);
    assert_eq!(http(504, None, &err("timeout_error", "Request timed out")).retry, Retry::Backoff);
    for (status, ty) in [
        (400, "invalid_request_error"),
        (402, "billing_error"),
        (403, "permission_error"),
        (404, "not_found_error"),
        (413, "request_too_large"),
    ] {
        let e = http(status, None, &err(ty, "no"));
        assert_eq!(e.retry, Retry::Never, "{ty}");
        assert!(!e.is_context_overflow(), "{ty}");
    }
    // A spend limit you set is a 400.
    let e = http(
        400,
        None,
        &err(
            "invalid_request_error",
            "You have reached your specified API usage limits. You will regain access on 2026-12-01 at 00:00 UTC.",
        ),
    );
    assert_eq!(e.retry, Retry::Never);
    // Anthropic's own message for an oversized image (recorded through OpenRouter, `metadata.raw`).
    let raw = crate::fixtures::case("msg_bad_image_claude")["body"]["metadata"]["raw"].as_str().unwrap().to_string();
    let e = http(400, None, &raw);
    assert!(e.message.contains("8000 pixels") && e.retry == Retry::Never && !e.is_context_overflow());
    // Unknown types: by status.
    assert_eq!(http(503, None, &err("mystery_error", "x")).retry, Retry::Backoff);
    assert_eq!(http(418, None, &err("mystery_error", "x")).retry, Retry::Never);
    // A non-JSON body (Cloudflare's 413 page).
    let e = http(413, None, "<html>413 Request Entity Too Large</html>");
    assert_eq!(
        (e.retry, e.details),
        (Retry::Never, Some(Value::String("<html>413 Request Entity Too Large</html>".into())))
    );
}

#[test]
fn context_overflows_are_recognized() {
    let e = http(
        400,
        None,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1048576 tokens > 1000000 maximum"},"request_id":"req_1"}"#,
    );
    assert!(e.is_context_overflow());
    assert_eq!(e.error_type.as_deref(), Some("context_length_exceeded"));
    // The prompt alone does not fit: shrinking the output budget cannot help.
    assert_eq!(crate::budget::shrink_after_overflow(128_000, &e.message), None);
    // max_tokens above the model's limit (an unknown model): the bot retries with less.
    let e = http(
        400,
        None,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 128000 > 64000, which is the maximum allowed number of output tokens for claude-sonnet-4-5-20250929"},"request_id":"req_1"}"#,
    );
    assert!(e.is_context_overflow());
    assert_eq!(e.error_type.as_deref(), Some("max_tokens_exceeded"));
    assert_eq!(crate::budget::shrink_after_overflow(128_000, &e.message), Some(64_000));
    let e = error_from_json(
        &json!({"type": "invalid_request_error", "message": "max_tokens: Field required"}),
        Some(400),
        None,
        ErrorKind::Http,
    );
    assert_eq!(e.error_type.as_deref(), Some("invalid_request_error"));
}

// ------------------------------------------------------------------ HTTP

struct Seen {
    head: String,
    body: Value,
}

/// One response of the fake API: status, extra headers, body.
type Response = (u16, Vec<(&'static str, String)>, String);

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e)).collect()
}

/// A fake API: answers each request in turn (`status`, extra headers, body),
/// one connection each, and records what was sent.
async fn serve(responses: Vec<Response>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for (status, headers, body) in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 16384];
            let (head, req) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                while buf.len() < pos + 4 + len {
                    let n = sock.read(&mut tmp).await.unwrap();
                    buf.extend_from_slice(&tmp[..n]);
                }
                break (head, serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null));
            };
            log.lock().unwrap().push(Seen { head, body: req });
            let ctype = if status == 200 { "text/event-stream" } else { "application/json" };
            let mut resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (k, v) in headers {
                resp.push_str(&format!("{k}: {v}\r\n"));
            }
            resp.push_str("\r\n");
            resp.push_str(&body);
            sock.write_all(resp.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}/v1"), seen)
}

async fn run(p: &Anthropic, prepared: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let mut events = Vec::new();
    let (r, o) = {
        let mut sink = |e: StreamEvent| events.push(e);
        p.stream(prepared, &mut sink, &NullObserver, &CancellationToken::new()).await
    };
    (r, o, events)
}

#[tokio::test]
async fn streams_with_the_documented_headers() {
    let events = [
        message_start("claude-opus-5", json!({"input_tokens": 10, "output_tokens": 1})),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hi"}}),
        json!({"type": "content_block_stop", "index": 0}),
        delta_end("end_turn", json!({"output_tokens": 2})),
        stop(),
    ];
    let (url, seen) = serve(vec![(200, vec![("request-id", "req_011CA".into())], sse(&events))]).await;
    let p = provider_at(&url, None);
    let o = opts(&["tier=fast"]);
    let h = [Turn::user_text("hi")];
    let prepared = p.prepare(&input("claude-opus-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, outcome, evs) = run(&p, &prepared).await;
    assert_eq!(
        r.unwrap(),
        json!({"role": "assistant", "id": "msg_01XFDUDYJgAACzvnptvVoYEL", "citations": true,
               "content": [{"type": "text", "text": "Hi"}]})
    );
    assert_eq!(outcome.upstream_id.as_deref(), Some("req_011CA"));
    assert!(evs.contains(&StreamEvent::Meta { upstream_id: Some("req_011CA".into()), provider: None, model: None }));
    let seen = seen.lock().unwrap();
    let head = seen[0].head.to_ascii_lowercase();
    assert!(head.starts_with("post /v1/messages "), "{head}");
    assert!(head.contains("x-api-key: sk-ant-test"));
    assert!(head.contains("anthropic-version: 2026-10-21"));
    assert!(head.contains("anthropic-beta: fast-mode-2026-02-01"), "{head}");
    assert_eq!(seen[0].body, prepared.body, "the body exactly as prepared");
}

#[tokio::test]
async fn a_paused_turn_is_continued_as_documented() {
    let first = [
        message_start("claude-sonnet-5-5", json!({"input_tokens": 100, "output_tokens": 1})),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_1", "name": "code_execution", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"code\":\"print(1)\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "pause_turn", "container": {"id": "container_abc", "expires_at": "2026-11-12T08:00:00Z"}}, "usage": {"output_tokens": 20}}),
        stop(),
    ];
    let second = [
        message_start("claude-sonnet-5-5", json!({"input_tokens": 130, "output_tokens": 1})),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "code_execution_tool_result", "tool_use_id": "srvtoolu_1", "content": {"type": "code_execution_result", "stdout": "1\n", "stderr": "", "return_code": 0, "content": []}}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "It printed 1."}}),
        json!({"type": "content_block_stop", "index": 1}),
        delta_end("end_turn", json!({"output_tokens": 10})),
        stop(),
    ];
    let (url, seen) = serve(vec![
        (200, vec![("request-id", "req_1".into())], sse(&first)),
        (200, vec![("request-id", "req_2".into())], sse(&second)),
    ])
    .await;
    let p = provider_at(&url, None);
    let o = opts(&["c"]);
    let h = [Turn::user_text("run it")];
    let prepared = p.prepare(&input("claude-sonnet-5-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, outcome, evs) = run(&p, &prepared).await;
    let native = r.unwrap();
    assert_eq!(native["hops"], json!([1, 2]));
    assert_eq!(native["tools"], prepared.body["tools"]);
    assert_eq!(outcome.upstream_id.as_deref(), Some("req_1,req_2"));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Notice(n) if n.contains("pause_turn"))));
    let acc = accumulate(&evs);
    assert_eq!(acc.upstream_id.as_deref(), Some("req_1,req_2"));
    assert_eq!(acc.native_finish_reason.as_deref(), Some("end_turn"));
    let u = acc.usage.unwrap();
    assert_eq!((u.input_tokens, u.output_tokens), (Some(230), Some(30)));
    let again = {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        seen[1].body.clone()
    };
    // Same request, the paused content appended as an assistant message, the container kept;
    // the paused message is the previous request for the cache diagnostics.
    assert_eq!(again["tools"], prepared.body["tools"]);
    assert_eq!(again["container"], "container_abc");
    assert_eq!(prepared.body["diagnostics"], json!({"previous_message_id": null}));
    assert_eq!(again["diagnostics"], json!({"previous_message_id": "msg_01XFDUDYJgAACzvnptvVoYEL"}));
    let msgs = again["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(
        msgs[1],
        json!({"role": "assistant", "content": [{"type": "server_tool_use", "id": "srvtoolu_1", "name": "code_execution", "input": {"code": "print(1)"}}]})
    );
    // The next turn replays the two requests as they were sent.
    let turn = assistant("claude-sonnet-5-5", native, "It printed 1.");
    let history = vec![Turn::user_text("run it"), turn, Turn::user_text("again")];
    let next = p.prepare(&input("claude-sonnet-5-5", &o, &history), &[], &Blobs::default()).await.unwrap();
    let m = next.body["messages"].as_array().unwrap();
    assert_eq!(m.len(), 4);
    assert_eq!(m[1], msgs[1]);
    assert_eq!(m[2]["content"][1]["text"], "It printed 1.");
}

#[tokio::test]
async fn the_pause_continuations_are_capped() {
    let paused = sse(&[
        message_start("claude-opus-5-5", json!({"input_tokens": 10, "output_tokens": 1})),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_x", "name": "web_search", "input": {"query": "q"}}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "pause_turn"}, "usage": {"output_tokens": 5}}),
        stop(),
    ]);
    let (url, seen) = serve((0..MAX_REQUESTS).map(|_| (200, vec![], paused.clone())).collect()).await;
    let p = provider_at(&url, None);
    let o = opts(&["s"]);
    let h = [Turn::user_text("q")];
    let prepared = p.prepare(&input("claude-opus-5-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, _, evs) = run(&p, &prepared).await;
    assert!(r.is_ok());
    assert_eq!(seen.lock().unwrap().len(), MAX_REQUESTS);
    let acc = accumulate(&evs);
    assert_eq!(
        (acc.finish_reason.as_deref(), acc.native_finish_reason.as_deref()),
        (Some("pause_turn"), Some("pause_turn"))
    );
    assert!(
        acc.notices.iter().any(|n| n.contains("after 5 request(s)") && n.contains("may be incomplete")),
        "{:?}",
        acc.notices
    );
}

#[tokio::test]
async fn http_errors_over_the_wire() {
    let body = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Number of requests has exceeded your per-minute rate limit"},"request_id":"req_9"}"#;
    let (url, _) =
        serve(vec![(429, vec![("retry-after", "12".into()), ("request-id", "req_9".into())], body.into())]).await;
    let p = provider_at(&url, None);
    let o = opts(&[]);
    let h = [Turn::user_text("hi")];
    let prepared = p.prepare(&input("claude-opus-5-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, outcome, _) = run(&p, &prepared).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry), (Some(429), Retry::After(12)));
    assert_eq!(outcome.upstream_id.as_deref(), Some("req_9"));
    // A stream that stops without message_stop is a dropped connection (retried).
    let cut = sse(&[message_start("claude-opus-5-5", json!({"input_tokens": 1, "output_tokens": 1}))]);
    let (url, _) = serve(vec![(200, vec![], cut)]).await;
    let p = provider_at(&url, None);
    let (r, _, _) = run(&p, &prepared).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry), (ErrorKind::Network, Retry::Backoff));
}

// ------------------------------------------------------------------ files, citations, diagnostics

/// A fake API answering by request line (`GET /v1/files/x`), one connection
/// each and concurrently (fetches run while the stream is read); records the
/// request lines and heads in order of arrival.
async fn serve_routes(routes: Vec<(&'static str, u16, &'static str, Vec<u8>)>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let routes = Arc::new(routes);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let routes = routes.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 16384];
                let head = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len: usize = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    while buf.len() < pos + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    break head;
                };
                let line: String = head.lines().next().unwrap_or("").split(' ').take(2).collect::<Vec<_>>().join(" ");
                log.lock().unwrap().push(head.clone());
                let (status, ctype, body) =
                    routes.iter().find(|(r, ..)| *r == line).map(|(_, s, c, b)| (*s, *c, b.clone())).unwrap_or((
                        404,
                        "application/json",
                        br#"{"type":"error","error":{"type":"not_found_error","message":"no route"}}"#.to_vec(),
                    ));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nrequest-id: req_files\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/v1"), seen)
}

fn file_meta(id: &str, name: &str, mime: &str, size: u64) -> Vec<u8> {
    json!({"id": id, "type": "file", "filename": name, "mime_type": mime, "size_bytes": size,
           "created_at": "2026-11-12T22:25:00.000000Z", "downloadable": true, "expires_at": null})
    .to_string()
    .into_bytes()
}

/// Turn 352's shape (Haiku 5.5 `+c`, "plot sin(x)"): the plot's `file_id`
/// arrives in the bash result mid-stream; the file is fetched and shown right
/// there, before the answer that follows it.
#[tokio::test]
async fn code_execution_files_are_fetched_where_they_were_reported() {
    let png = b"\x89PNG\r\n\x1a\n-sin-plot".to_vec();
    let csv = b"x,sin(x)\n0,0\n".to_vec();
    let result = json!({"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_01", "content": {
        "type": "bash_code_execution_result",
        "stdout": "Copied image to /files/output/f674dc156d3ddd58/sin_plot.png\n", "stderr": "", "return_code": 0,
        "content": [{"type": "bash_code_execution_output", "file_id": "file_01RWNjFD5Lh7fs5XXFzxJzHy"},
                    {"type": "bash_code_execution_output", "file_id": "file_csv"}]}});
    let events = [
        message_start(
            "claude-haiku-5-5",
            json!({"input_tokens": 4, "cache_creation_input_tokens": 2910, "output_tokens": 4}),
        ),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_01", "name": "bash_code_execution", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"python /tmp/plot_sin.py\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": result}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "It's exported as `sin_plot.png`."}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": null,
               "container": {"id": "container_01P9V7", "expires_at": "2026-11-12T23:25:42.169215Z"}},
               "usage": {"output_tokens": 7449, "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0}}}),
        stop(),
    ];
    let (url, seen) = serve_routes(vec![
        ("POST /v1/messages", 200, "text/event-stream", sse(&events).into_bytes()),
        (
            "GET /v1/files/file_01RWNjFD5Lh7fs5XXFzxJzHy",
            200,
            "application/json",
            file_meta("file_01RWNjFD5Lh7fs5XXFzxJzHy", "sin_plot.png", "image/png", png.len() as u64),
        ),
        ("GET /v1/files/file_01RWNjFD5Lh7fs5XXFzxJzHy/content", 200, "image/png", png.clone()),
        ("GET /v1/files/file_csv", 200, "application/json", file_meta("file_csv", "data.csv", "text/csv", 14)),
        ("GET /v1/files/file_csv/content", 200, "text/csv", csv.clone()),
    ])
    .await;
    let p = provider_at(&url, None);
    let o = opts(&["c"]);
    let h = [Turn::user_text("plot sin(x) and show the image")];
    let prepared = p.prepare(&input("claude-haiku-5-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, _, evs) = run(&p, &prepared).await;
    let native = r.unwrap();
    // The result block goes back verbatim (its file ids, no metadata added).
    assert_eq!(native["content"][1], result);
    let at = |pred: &dyn Fn(&StreamEvent) -> bool| evs.iter().position(pred).unwrap_or_else(|| panic!("{evs:#?}"));
    let result_at = at(&|e| matches!(e, StreamEvent::Tool(t) if t.status.as_deref() == Some("completed")));
    let image_at = at(&|e| matches!(e, StreamEvent::Image { data, mime } if *data == png && mime == "image/png"));
    let file_at = at(
        &|e| matches!(e, StreamEvent::File { data, mime, name } if *data == csv && mime == "text/csv" && name.as_deref() == Some("data.csv")),
    );
    let text_at = at(&|e| matches!(e, StreamEvent::Text(_)));
    assert!(result_at < image_at && image_at < file_at && file_at < text_at, "{evs:#?}");
    // The call's output names the files (Files API metadata), in place.
    let update = evs[result_at + 1..image_at]
        .iter()
        .find_map(|e| if let StreamEvent::Tool(t) = e { Some(t) } else { None })
        .expect("the call with the files' metadata");
    let items = update.output.as_ref().unwrap()["content"].as_array().unwrap().clone();
    assert_eq!(items[0]["file"]["filename"], "sin_plot.png");
    assert_eq!(items[1]["file"]["mime_type"], "text/csv");
    assert_eq!(items[0]["file_id"], "file_01RWNjFD5Lh7fs5XXFzxJzHy");
    let acc = accumulate(&evs);
    let Part::ToolCall(call) = &acc.parts()[0] else { panic!("{:?}", acc.parts()) };
    assert_eq!(call.output.as_ref().unwrap()["content"][0]["file"]["size_bytes"], png.len());
    assert_eq!(
        call.output.as_ref().unwrap()["stdout"],
        "Copied image to /files/output/f674dc156d3ddd58/sin_plot.png\n"
    );
    assert!(acc.notices.is_empty(), "{:?}", acc.notices);
    // The Files API calls: the key and version, no beta header.
    let seen = seen.lock().unwrap();
    let files: Vec<&String> = seen.iter().filter(|h| h.starts_with("GET /v1/files/")).collect();
    assert_eq!(files.len(), 4, "{seen:#?}");
    for h in files {
        let h = h.to_ascii_lowercase();
        assert!(h.contains("x-api-key: sk-ant-test") && h.contains("anthropic-version: 2026-10-21"), "{h}");
        assert!(!h.contains("anthropic-beta"), "{h}");
    }
}

#[tokio::test]
async fn files_that_cannot_be_fetched_are_notices() {
    let result = json!({"type": "bash_code_execution_tool_result", "tool_use_id": "srvtoolu_02", "content": {
        "type": "bash_code_execution_result", "stdout": "", "stderr": "", "return_code": 0,
        "content": [{"type": "bash_code_execution_output", "file_id": "file_big"},
                    {"type": "bash_code_execution_output", "file_id": "file_gone"}]}});
    let events = [
        message_start("claude-haiku-5-5", json!({"input_tokens": 4, "output_tokens": 4})),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_02", "name": "bash_code_execution", "input": {"command": "make"}}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": result}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "Done."}}),
        json!({"type": "content_block_stop", "index": 2}),
        delta_end("end_turn", json!({"output_tokens": 10})),
        stop(),
    ];
    let (url, _) = serve_routes(vec![
        ("POST /v1/messages", 200, "text/event-stream", sse(&events).into_bytes()),
        ("GET /v1/files/file_big", 200, "application/json", file_meta("file_big", "video.mp4", "video/mp4", 60 << 20)),
        (
            "GET /v1/files/file_gone",
            404,
            "application/json",
            br#"{"type":"error","error":{"type":"not_found_error","message":"File `file_gone` not found."},"request_id":"req_011CQFYcrRp7mCHLDsAYT8Qt"}"#.to_vec(),
        ),
    ])
    .await;
    let p = provider_at(&url, None);
    let o = opts(&["c"]);
    let h = [Turn::user_text("render")];
    let prepared = p.prepare(&input("claude-haiku-5-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, _, evs) = run(&p, &prepared).await;
    assert!(r.is_ok(), "a file that cannot be fetched is no failure of the answer");
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Image { .. } | StreamEvent::File { .. })));
    let acc = accumulate(&evs);
    assert_eq!(
        acc.notices,
        vec![
            "the file video.mp4 (file_big) that code execution wrote was not downloaded: 60.0 MB is more than the \
             bot's limit of 50.0 MB",
            "the file file_gone that code execution wrote was not downloaded: HTTP 404: File `file_gone` not found.",
        ]
    );
    assert_eq!(acc.turn().answer_text(), "Done.");
}

#[test]
fn file_ids_of_every_code_result_kind() {
    let out = |ty: &str, item: &str| json!({"type": ty, "stderr": "", "return_code": 0, "content": [{"type": item, "file_id": "file_1"}]});
    for (ty, item) in [
        ("bash_code_execution_result", "bash_code_execution_output"),
        ("code_execution_result", "code_execution_output"),
        ("encrypted_code_execution_result", "code_execution_output"),
    ] {
        assert_eq!(files::file_ids(&out(ty, item)), vec!["file_1"], "{ty}");
    }
    assert!(files::file_ids(&json!({"type": "bash_code_execution_result", "content": []})).is_empty());
    assert!(files::file_ids(&json!([{"type": "web_search_result", "url": "https://x"}])).is_empty());
}

/// Programmatic tool calling and dynamic filtering (turn 292): a call made
/// from inside a code run carries its `caller`, which goes into the input.
#[test]
fn calls_made_by_code_execution_carry_their_caller() {
    let mut p = Parser::new(spec("claude-opus-5-5"));
    let caller = json!({"type": "code_execution_20260120", "tool_id": "srvtoolu_01KBhi"});
    let evs = feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 10, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "srvtoolu_01KBhi", "name": "code_execution", "input": {}, "caller": {"type": "direct"}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"code\": \"r = await web_fetch({'url': 'https://www.rust-lang.org/'})\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srvtoolu_02", "name": "web_fetch", "input": {"url": "https://www.rust-lang.org/"}, "caller": caller}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_02", "caller": caller, "content": {
                "type": "web_fetch_result", "url": "https://www.rust-lang.org/", "retrieved_at": "2026-11-12T05:15:38+00:00",
                "content": {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "Rust"}, "title": "Rust Programming Language"}}}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "code_execution_tool_result", "tool_use_id": "srvtoolu_01KBhi", "content": {
                "type": "code_execution_result", "stdout": "1.100", "stderr": "", "return_code": 0, "content": [], "abort_reason": null}}}),
            json!({"type": "content_block_stop", "index": 3}),
        ],
    );
    let acc = accumulate(&evs);
    let calls: Vec<&ToolCall> =
        acc.parts().iter().filter_map(|p| if let Part::ToolCall(t) = p { Some(t) } else { None }).collect();
    assert_eq!(calls[0].input, Some(json!({"code": "r = await web_fetch({'url': 'https://www.rust-lang.org/'})"})));
    assert_eq!(calls[1].input, Some(json!({"url": "https://www.rust-lang.org/", "caller": caller})));
    assert_eq!(calls[1].output.as_ref().unwrap()["retrieved_at"], "2026-11-12T05:15:38+00:00");
    // The code's result as sent, undocumented fields included.
    assert_eq!(
        calls[0].output,
        Some(
            json!({"type": "code_execution_result", "stdout": "1.100", "stderr": "", "return_code": 0, "content": [], "abort_reason": null})
        )
    );
    // Nothing of this goes into the replayed blocks.
    assert!(p.content()[1].get("caller") == Some(&caller) && p.content()[1]["input"].get("caller").is_none());
}

#[test]
fn viewed_images_and_errors_stay_as_sent() {
    let mut p = Parser::new(spec("claude-sonnet-5-5"));
    let evs = feed(
        &mut p,
        &[
            message_start("claude-sonnet-5-5", json!({"input_tokens": 10, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text_editor_code_execution_tool_result", "tool_use_id": "srvtoolu_v", "content": {
                "type": "text_editor_code_execution_view_result", "file_type": "image", "content": "iVBORw0KGgoAAAANSUhEUg", "num_lines": null}}}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "web_search_tool_result", "tool_use_id": "srvtoolu_s", "content": {
                "type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}}}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text_editor_code_execution_tool_result", "tool_use_id": "srvtoolu_e", "content": {
                "type": "text_editor_code_execution_tool_result_error", "error_code": "invalid_tool_input", "error_message": "path must be absolute"}}}),
        ],
    );
    let out: Vec<(Option<&str>, &Value)> = evs
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Tool(t) => Some((t.status.as_deref(), t.output.as_ref().unwrap())),
            _ => None,
        })
        .collect();
    assert_eq!(out[0].1["content"], "(22 characters, sent back to the model in later turns; not shown)");
    assert_eq!(out[0].1["file_type"], "image");
    assert_eq!(
        out[1],
        (Some("failed"), &json!({"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}))
    );
    assert_eq!(out[2].1["error_message"], "path must be absolute");
}

/// Citations of the documents a request has (user documents, pages fetched
/// earlier) and of a page fetched in the stream, per the citations guide's shapes.
#[test]
fn document_and_fetched_page_citations() {
    let body = json!({"messages": [
        {"role": "user", "content": [
            {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "The grass is green. The sky is blue."}, "title": "notes.txt", "citations": {"enabled": true}},
            {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0x"}, "citations": {"enabled": true}},
            {"type": "text", "text": "Summarize, and read https://example.com/a"}]},
    ], "tools": [{"type": "web_fetch_20260318", "name": "web_fetch", "citations": {"enabled": true}}]});
    let mut p = Parser::with_request(spec("claude-opus-5-5"), &body);
    let char_loc = json!({"type": "char_location", "cited_text": "The grass is green. ", "document_index": 0,
        "document_title": "notes.txt", "start_char_index": 0, "end_char_index": 20, "file_id": null});
    let page_loc = json!({"type": "page_location", "cited_text": "Water is essential for life.", "document_index": 1,
        "document_title": null, "start_page_number": 5, "end_page_number": 6});
    let fetched = json!({"type": "char_location", "cited_text": "Example A is an example.", "document_index": 2,
        "document_title": "Example A", "start_char_index": 0, "end_char_index": 24});
    let by_title = json!({"type": "char_location", "cited_text": "is an example", "document_index": 7,
        "document_title": "Example A", "start_char_index": 10, "end_char_index": 23});
    let blocks = json!({"type": "content_block_location", "cited_text": "These are important findings.", "document_index": 0,
        "document_title": "notes.txt", "start_block_index": 0, "end_block_index": 1});
    let delta = |i: u64, c: &Value| json!({"type": "content_block_delta", "index": i, "delta": {"type": "citations_delta", "citation": c}});
    let evs = feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 10, "output_tokens": 1})),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "", "citations": []}}),
            delta(0, &char_loc),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "the grass is green"}}),
            delta(0, &page_loc),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "server_tool_use", "id": "srvtoolu_f", "name": "web_fetch", "input": {"url": "https://example.com/a"}}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "web_fetch_tool_result", "tool_use_id": "srvtoolu_f", "content": {
                "type": "web_fetch_result", "url": "https://example.com/a", "retrieved_at": "2026-11-13T01:00:00Z",
                "content": {"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "Example A is an example."}, "title": "Example A", "citations": {"enabled": true}}}}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "Example A is an example"}}),
            delta(3, &fetched),
            delta(3, &by_title),
            delta(3, &blocks),
            json!({"type": "content_block_stop", "index": 3}),
        ],
    );
    let cites: Vec<Citation> = evs
        .iter()
        .filter_map(|e| if let StreamEvent::Citations(c) = e { Some(c.clone()) } else { None })
        .flatten()
        .collect();
    assert_eq!(cites.len(), 5);
    // A user document: its title, the cited text, the location in `extra` (no URL).
    assert_eq!(
        cites[0],
        Citation {
            url: None,
            title: Some("notes.txt".into()),
            cited_text: Some("The grass is green. ".into()),
            range: None,
            extra: Some(json!({"type": "char_location", "document_index": 0, "document_title": "notes.txt",
                               "start_char_index": 0, "end_char_index": 20, "file_id": null})),
        }
    );
    // A PDF without a title: pages 5 to 6 (exclusive), no title to show.
    assert_eq!((cites[1].url.as_deref(), cites[1].title.as_deref()), (None, None));
    assert_eq!(cites[1].extra.as_ref().unwrap()["start_page_number"], 5);
    // The page fetched in this turn: its URL.
    assert_eq!(
        (cites[2].url.as_deref(), cites[2].title.as_deref()),
        (Some("https://example.com/a"), Some("Example A"))
    );
    // An index that does not fit, a title only one document has.
    assert_eq!(cites[3].url.as_deref(), Some("https://example.com/a"));
    assert_eq!(cites[4].extra.as_ref().unwrap()["end_block_index"], 1);
    // All on the answer text, in order; the blocks keep them verbatim for replay.
    let acc = accumulate(&evs);
    let texts: Vec<usize> = acc
        .parts()
        .iter()
        .filter_map(|p| if let Part::Text { citations, .. } = p { Some(citations.len()) } else { None })
        .collect();
    assert_eq!(texts, vec![2, 3]);
    assert_eq!(p.content()[3]["citations"][0], fetched);
    assert_eq!(p.native_payload(None)["citations"], true);
}

#[tokio::test]
async fn citations_stay_off_in_conversations_made_without_them() {
    let mut blobs = Blobs::default();
    let pdf = blobs.add(b"%PDF-1.7 old", "application/pdf");
    let doc_turn = Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![Part::File { blob: pdf, filename: Some("old.pdf".into()), generated: false }, Part::text("read")],
        native: None,
    };
    let fetch = json!([{"type": "web_fetch_20260318", "name": "web_fetch"}]);
    let legacy = json!({"role": "assistant", "tools": fetch, "content": [
        {"type": "thinking", "thinking": "", "signature": "sig"}, {"type": "text", "text": "It says hi."}]});
    let history =
        vec![doc_turn.clone(), assistant("claude-opus-5-5", legacy.clone(), "It says hi."), Turn::user_text("more")];
    let o = opts(&["u"]);
    let p = provider().prepare(&input("claude-opus-5-5", &o, &history), &[], &blobs).await.unwrap();
    // As before: no citations on the document or on web fetch, so the old answer replays natively.
    assert!(p.body["messages"][0]["content"][0].get("citations").is_none());
    assert_eq!(p.body["tools"], fetch);
    assert_eq!(p.body["messages"][1]["content"][0]["signature"], "sig");
    assert!(p.notes.iter().any(|n| n.contains("without citations")), "{:?}", p.notes);
    assert!(!super::stream::citations_on(&p.body));
    // Answers made with citations on: on.
    let mut current = legacy.clone();
    current["citations"] = json!(true);
    current["tools"] = json!([{"type": "web_fetch_20260318", "name": "web_fetch", "citations": {"enabled": true}}]);
    let history = vec![doc_turn.clone(), assistant("claude-opus-5-5", current, "It says hi."), Turn::user_text("more")];
    let p = provider().prepare(&input("claude-opus-5-5", &o, &history), &[], &blobs).await.unwrap();
    assert_eq!(p.body["messages"][0]["content"][0]["citations"], json!({"enabled": true}));
    assert_eq!(p.body["messages"][1]["content"][0]["signature"], "sig");
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    // An old answer before any document, or one of another model: no reason to keep them off.
    let history = vec![Turn::user_text("hi"), assistant("claude-opus-5-5", legacy, "Hi."), doc_turn];
    let p = provider().prepare(&input("claude-opus-5-5", &o, &history), &[], &blobs).await.unwrap();
    assert_eq!(p.body["messages"][2]["content"][0]["citations"], json!({"enabled": true}));
}

#[tokio::test]
async fn cache_diagnostics_name_the_previous_answer() {
    // The first turn opts in with null.
    let p = prep_ok("claude-opus-5-5", &[]).await;
    assert_eq!(p.body["diagnostics"], json!({"previous_message_id": null}));
    assert!(p.applied.iter().any(|a| a.patch.get("diagnostics").is_some()));
    // Later: the latest answer of the same model on this API.
    let earlier = assistant(
        "claude-opus-5-5",
        json!({"role": "assistant", "id": "msg_1", "content": [{"type": "text", "text": "a"}]}),
        "a",
    );
    let later = assistant(
        "claude-opus-5-5",
        json!({"role": "assistant", "id": "msg_2", "content": [{"type": "text", "text": "b"}]}),
        "b",
    );
    let mut other = assistant("claude-sonnet-5-5", json!({"role": "assistant", "id": "msg_3", "content": []}), "c");
    other.native.as_mut().unwrap().model = "claude-sonnet-5-5".into();
    let history = vec![
        Turn::user_text("1"),
        earlier,
        Turn::user_text("2"),
        later,
        Turn::user_text("3"),
        other,
        Turn::user_text("4"),
    ];
    let p = prep("claude-opus-5-5", &[], &history).await.unwrap();
    assert_eq!(p.body["diagnostics"], json!({"previous_message_id": "msg_2"}));
    // A miss the API attributes to a change is a notice; the other results are not.
    let start = |d: Value| {
        let mut m = message_start(
            "claude-opus-5-5",
            json!({"input_tokens": 42, "cache_creation_input_tokens": 41850, "output_tokens": 1}),
        );
        m["message"]["diagnostics"] = d;
        m
    };
    let notices = |d: Value| -> Vec<String> {
        let mut p = Parser::new(spec("claude-opus-5-5"));
        feed(&mut p, &[start(d)])
            .into_iter()
            .filter_map(|e| if let StreamEvent::Notice(n) = e { Some(n) } else { None })
            .collect()
    };
    assert_eq!(
        notices(json!({"cache_miss_reason": {"type": "messages_changed", "cache_missed_input_tokens": 41850}})),
        vec![
            "Anthropic's cache diagnostics: messages_changed (an earlier message differs from the previous request's); \
             about 41,850 input tokens were not read from the cache"
        ]
    );
    assert_eq!(
        notices(json!({"cache_miss_reason": {"type": "tools_changed", "cache_missed_input_tokens": 7}})).len(),
        1
    );
    for quiet in [
        json!(null),
        json!({"cache_miss_reason": null}),
        json!({"cache_miss_reason": {"type": "previous_message_not_found"}}),
        json!({"cache_miss_reason": {"type": "unavailable"}}),
    ] {
        assert!(notices(quiet.clone()).is_empty(), "{quiet}");
    }
}

#[tokio::test]
async fn a_refusal_fails_with_its_category() {
    let events = [
        message_start("claude-opus-5-5", json!({"input_tokens": 272, "output_tokens": 0})),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_sequence": null, "stop_details": {
            "type": "refusal", "category": "bio", "explanation": "API integrators: you can reduce refusals for your users by configuring a fallback model — see https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback"}},
            "usage": {"output_tokens": 272, "output_tokens_details": {"thinking_tokens": 272}}}),
        stop(),
    ];
    let (url, _) = serve(vec![(200, vec![("request-id", "req_r".into())], sse(&events))]).await;
    let p = provider_at(&url, None);
    let o = opts(&[]);
    let h = [Turn::user_text("the NATO alphabet")];
    let prepared = p.prepare(&input("claude-opus-5-5", &o, &h), &[], &Blobs::default()).await.unwrap();
    let (r, outcome, evs) = run(&p, &prepared).await;
    let e = r.unwrap_err();
    assert_eq!(e.message, "refusal (bio)");
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("content_filter")));
    assert_eq!(e.details.as_ref().unwrap()["category"], "bio");
    assert!(e.details.as_ref().unwrap()["explanation"].as_str().unwrap().starts_with("API integrators"));
    assert_eq!(outcome.upstream_id.as_deref(), Some("req_r"));
    // Billed (bio is billed before any output) and finished as a content filter stop.
    let acc = accumulate(&evs);
    assert_eq!(acc.finish_reason.as_deref(), Some("content_filter"));
    assert!(acc.usage.unwrap().cost_usd.unwrap() > 0.0);
    // Without a category.
    let mut p = Parser::new(spec("claude-opus-5-5"));
    feed(
        &mut p,
        &[
            message_start("claude-opus-5-5", json!({"input_tokens": 1, "output_tokens": 0})),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": null, "explanation": null}}, "usage": {"output_tokens": 0}}),
            stop(),
        ],
    );
    assert_eq!(p.refusal().unwrap().message, "refusal");
}
