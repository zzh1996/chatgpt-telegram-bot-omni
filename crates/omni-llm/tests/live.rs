//! Live tests against the real OpenRouter API. Ignored by default:
//!
//! ```sh
//! cargo test -p omni-llm --test live -- --ignored --nocapture
//! OPENROUTER_API_KEY=sk-... cargo test -p omni-llm --test live -- --ignored --nocapture
//! ```

use std::time::Duration;

use omni_core::Turn;
use omni_core::trigger::OptionToken;
use omni_llm::openrouter::options::FidelityConfig;
use omni_llm::openrouter::{OpenRouter, OpenRouterConfig};
use omni_llm::{BlobLoader, BoxFuture, CallInput, NullObserver, StreamEvent};
use tokio_util::sync::CancellationToken;

struct NoBlobs;
impl BlobLoader for NoBlobs {
    fn load<'a>(&'a self, _: &'a omni_core::BlobRef) -> BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async { anyhow::bail!("no blobs") })
    }
}

fn client() -> OpenRouter {
    OpenRouter::new(OpenRouterConfig {
        api_key: std::env::var("OPENROUTER_API_KEY").unwrap_or_default(),
        base_url: "https://openrouter.ai/api/v1".into(),
        http_referer: None,
        x_title: None,
        connect_timeout: Duration::from_secs(15),
        idle_timeout: Duration::from_secs(600),
        fidelity: FidelityConfig::standard(),
        echo_upstream_body: true,
        router_metadata: true,
    })
    .unwrap()
}

#[tokio::test]
#[ignore]
async fn live_catalog_and_endpoints() {
    let or = client();
    let n = or.refresh_catalog().await.unwrap();
    println!("catalog: {n} models");
    assert!(n > 100);
    let m = or.catalog.get("anthropic/claude-opus-5.5").expect("claude opus 5.5 in catalog");
    println!("opus 5.5 max effort {:?}, max out {:?}", m.max_effort(), m.top_provider.max_completion_tokens);
    let eps = or.endpoints("anthropic/claude-opus-5.5").await.unwrap();
    for e in eps.iter() {
        println!(
            "  {:?} quant={:?} native_search={}",
            e.tag,
            e.quantization,
            e.has_native_tool("openrouter:web_search")
        );
    }
    assert!(eps.iter().any(|e| e.base_slug() == Some("anthropic")));
}

#[tokio::test]
#[ignore]
async fn live_stream_small_completion() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        eprintln!("OPENROUTER_API_KEY not set; skipping");
        return;
    }
    let or = client();
    or.refresh_catalog().await.unwrap();
    let model = std::env::var("LIVE_MODEL").unwrap_or_else(|_| "anthropic/claude-haiku-5.5".into());
    let history = vec![Turn::user_text("Reply with exactly: pong")];
    let opts =
        vec![OptionToken::KeyValue("e".into(), "low".into()), OptionToken::KeyValue("max".into(), "2000".into())];
    let input = CallInput { model: &model, options: &opts, history: &history, session_id: None, user: None };
    let p = or.prepare_chat(&input, &[], &NoBlobs).await.unwrap();
    println!("request: {}", serde_json::to_string_pretty(&p.body).unwrap());
    let mut text = String::new();
    let mut sink = |e: StreamEvent| {
        if let StreamEvent::Text(t) = &e {
            text.push_str(t);
        }
        println!("event: {e:?}");
    };
    let (r, outcome) = or.stream_chat(&p, &mut sink, &NullObserver, &CancellationToken::new()).await;
    println!("upstream echo: {}", serde_json::to_string_pretty(&outcome.upstream_echo).unwrap());
    println!("router metadata: {:?}", outcome.router_metadata);
    let native = r.unwrap();
    println!("native: {native}");
    assert!(text.to_lowercase().contains("pong"));
}
