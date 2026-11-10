//! End-to-end tests of the OpenRouter streaming client against a local fake server
//! that writes raw HTTP/SSE with controlled chunking and delays.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::Turn;
use omni_llm::openrouter::options::FidelityConfig;
use omni_llm::openrouter::{OpenRouter, OpenRouterConfig};
use omni_llm::{BlobLoader, BoxFuture, CallInput, ErrorKind, NullObserver, Retry, StreamEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct NoBlobs;
impl BlobLoader for NoBlobs {
    fn load<'a>(&'a self, _: &'a omni_core::BlobRef) -> BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async { anyhow::bail!("no blobs") })
    }
}

enum Step {
    Bytes(String),
    Sleep(u64),
}

/// Serve one request: respond with `status` and the given body steps (chunked).
async fn serve(status: u16, steps: Vec<Step>) -> (String, Arc<Mutex<Option<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        // Read headers + body (Content-Length).
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
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
                let body: Value = serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null);
                *cap.lock().unwrap() = Some(body);
                break;
            }
        }
        let ctype = if status == 200 { "text/event-stream" } else { "application/json" };
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\nx-generation-id: gen-test\r\ntransfer-encoding: chunked\r\n\r\n"
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        for s in steps {
            match s {
                Step::Bytes(b) => {
                    let chunk = format!("{:x}\r\n{}\r\n", b.len(), b);
                    if sock.write_all(chunk.as_bytes()).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                }
                Step::Sleep(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            }
        }
        let _ = sock.write_all(b"0\r\n\r\n").await;
    });
    (format!("http://{addr}/api/v1"), captured)
}

fn client(base: String, idle_ms: u64) -> OpenRouter {
    let or = OpenRouter::new(OpenRouterConfig {
        api_key: "test".into(),
        base_url: base,
        http_referer: None,
        x_title: None,
        connect_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_millis(idle_ms),
        fidelity: FidelityConfig::standard(),
        echo_upstream_body: true,
        router_metadata: true,
    })
    .unwrap();
    // Mark the (empty) catalog as fresh so no /models request hits the fake server.
    or.catalog.replace(Vec::new());
    or
}

fn data(v: Value) -> Step {
    Step::Bytes(format!("data: {v}\n\n"))
}

async fn call(or: &OpenRouter) -> (Result<Value, omni_llm::LlmError>, Vec<StreamEvent>) {
    let history = vec![Turn::user_text("hi")];
    let input = CallInput {
        model: "anthropic/claude-test",
        options: &[],
        history: &history,
        session_id: Some("tg-1".into()),
        user: None,
    };
    let p = or.prepare_chat(&input, &[], &NoBlobs).await.unwrap();
    let mut events = Vec::new();
    let mut sink = |e: StreamEvent| events.push(e);
    let (r, _) = or.stream_chat(&p, &mut sink, &NullObserver, &CancellationToken::new()).await;
    (r, events)
}

#[tokio::test]
async fn normal_stream_with_keepalive_and_usage() {
    let (base, captured) = serve(
        200,
        vec![
            Step::Bytes(": OPENROUTER PROCESSING\n\n".into()),
            data(json!({"id": "gen-test", "provider": "Anthropic", "model": "anthropic/claude-test", "choices": [{"index": 0, "delta": {"role": "assistant", "content": "", "reasoning_details": [{"type": "reasoning.text", "text": "think", "index": 0}]}}]})),
            Step::Sleep(30),
            data(json!({"id": "gen-test", "choices": [{"index": 0, "delta": {"content": "Hel"}}]})),
            data(json!({"id": "gen-test", "choices": [{"index": 0, "delta": {"content": "lo"}, "finish_reason": "stop", "native_finish_reason": "end_turn"}]})),
            data(json!({"id": "gen-test", "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 3, "completion_tokens": 4, "cost": 0.5}})),
            Step::Bytes("data: [DONE]\n\n".into()),
        ],
    )
    .await;
    let or = client(base, 5_000);
    let (r, events) = call(&or).await;
    let native = r.expect("success");
    assert_eq!(native["content"], "Hello");
    assert_eq!(native["reasoning_details"][0]["text"], "think");
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Usage(u) if u.cost_usd == Some(0.5))));
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Meta { upstream_id: Some(id), .. } if id == "gen-test")));
    // Fidelity defaults were sent.
    let body = captured.lock().unwrap().clone().unwrap();
    assert_eq!(body["provider"]["only"], json!(["anthropic"]));
    assert_eq!(body["provider"]["allow_fallbacks"], json!(false));
    assert_eq!(body["plugins"][0]["id"], "context-compression");
    assert_eq!(body["debug"]["echo_upstream_body"], json!(true));
    assert_eq!(body["session_id"], "tg-1");
    assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
}

#[tokio::test]
async fn mid_stream_error_is_reported_verbatim() {
    let (base, _) = serve(
        200,
        vec![
            data(json!({"id": "gen-test", "choices": [{"index": 0, "delta": {"content": "partial"}}]})),
            data(json!({"id": "gen-test", "error": {"code": 502, "message": "Upstream died", "metadata": {"error_type": "provider_unavailable", "provider_name": "Anthropic", "raw": {"type": "overloaded_error"}}}, "choices": [{"index": 0, "delta": {"content": ""}, "finish_reason": "error"}]})),
        ],
    )
    .await;
    let (r, events) = call(&client(base, 5_000)).await;
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::InBand);
    assert_eq!(e.retry, Retry::Backoff);
    assert!(e.partial_output);
    assert_eq!(e.details.as_ref().unwrap()["error"]["metadata"]["raw"]["type"], "overloaded_error");
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Text(t) if t == "partial")));
}

#[tokio::test]
async fn http_error_is_permanent_and_preserved() {
    let body = json!({"error": {"code": 400, "message": "This model's maximum context length is ...", "metadata": {"error_type": "context_length_exceeded", "provider_name": "Anthropic"}}});
    let (base, _) = serve(400, vec![Step::Bytes(body.to_string())]).await;
    let (r, _) = call(&client(base, 5_000)).await;
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Http);
    assert_eq!(e.status, Some(400));
    assert_eq!(e.retry, Retry::Never);
    assert_eq!(e.details.unwrap(), body);
}

#[tokio::test]
async fn truncated_stream_is_a_retryable_network_error() {
    let (base, _) =
        serve(200, vec![data(json!({"id": "gen-test", "choices": [{"index": 0, "delta": {"content": "par"}}]}))]).await;
    let (r, _) = call(&client(base, 5_000)).await;
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Network);
    assert!(e.is_retryable());
    assert!(e.partial_output);
}

#[tokio::test]
async fn idle_timeout() {
    let (base, _) = serve(
        200,
        vec![data(json!({"id": "gen-test", "choices": [{"index": 0, "delta": {"content": "x"}}]})), Step::Sleep(3_000)],
    )
    .await;
    let (r, _) = call(&client(base, 300)).await;
    let e = r.unwrap_err();
    assert_eq!(e.kind, ErrorKind::Timeout);
    assert!(e.is_retryable());
}
