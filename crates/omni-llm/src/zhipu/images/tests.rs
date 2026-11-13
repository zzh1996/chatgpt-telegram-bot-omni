//! Zhipu image tests: requests per model and option, the input rules, and the
//! task / direct flows against a local fake server (shapes of Zhipu's docs).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use omni_core::content::{Part, Role};
use omni_core::trigger::OptionToken;
use omni_core::{BlobRef, Origin, Turn};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::BlobLoader;
use crate::provider::Provider;

struct NoBlobs;
impl BlobLoader for NoBlobs {
    fn load<'a>(&'a self, _r: &'a BlobRef) -> crate::BoxFuture<'a, anyhow::Result<Vec<u8>>> {
        Box::pin(async { anyhow::bail!("no blobs") })
    }
}

fn zp_at(base: &str) -> super::super::Zhipu {
    super::super::Zhipu::new(DirectConfig {
        api_key: "k".into(),
        base_url: base.into(),
        connect_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(5),
        max_tokens_cap: None,
        cny_per_usd: 7.0,
        long_decode_budget: 393_216,
    })
    .unwrap()
}

fn zp() -> super::super::Zhipu {
    zp_at("https://open.bigmodel.cn/api/paas/v4")
}

fn kv(k: &str, v: &str) -> OptionToken {
    OptionToken::KeyValue(k.into(), v.into())
}

fn flag(f: &str) -> OptionToken {
    OptionToken::Flag(f.into())
}

fn input<'a>(model: &'a str, options: &'a [OptionToken], history: &'a [Turn]) -> CallInput<'a> {
    CallInput {
        model,
        options,
        history,
        session_id: None,
        user: Some("user-hash-1".into()),
        known_context: None,
        max_tokens_override: None,
    }
}

async fn prep_h(model: &str, options: &[OptionToken], history: &[Turn]) -> Result<Prepared, LlmError> {
    zp().prepare(&input(model, options, history), &[], &NoBlobs).await
}

async fn prep(model: &str, options: &[OptionToken]) -> Result<Prepared, LlmError> {
    prep_h(model, options, &[Turn::user_text("一只猫")]).await
}

#[tokio::test]
async fn defaults_are_the_largest_size_hd_without_watermark() {
    let p = prep("glm-image", &[]).await.unwrap();
    assert_eq!(p.dialect, DIALECT);
    assert_eq!(p.url, "https://open.bigmodel.cn/api/paas/v4/async/images/generations");
    assert_eq!(
        p.body,
        json!({"model": "glm-image", "prompt": "一只猫", "quality": "hd", "size": "2048x2048",
               "watermark_enabled": false, "user_id": "user-hash-1"})
    );
    assert_eq!(p.settings, vec!["quality hd", "size 2048x2048", "no watermark"]);
    assert!(p.warnings.is_empty() && p.notes.is_empty());
    let p = prep("cogview-4-250304", &[]).await.unwrap();
    assert_eq!(p.url, "https://open.bigmodel.cn/api/paas/v4/images/generations");
    assert_eq!((p.body["quality"].as_str(), p.body["size"].as_str()), (Some("hd"), Some("1440x1440")));
    assert!(zp().knows("cogview-3-flash") && zp().knows("glm-image"));
    // The chat models are unchanged.
    assert_eq!(prep("glm-5.3", &[]).await.unwrap().dialect, super::super::DIALECT);
}

#[tokio::test]
async fn sizes_ratios_and_quality() {
    let size = |p: &Prepared| p.body["size"].as_str().unwrap().to_string();
    assert_eq!(size(&prep("glm-image", &[kv("ar", "16:9")]).await.unwrap()), "2048x1152");
    assert_eq!(size(&prep("glm-image", &[kv("ar", "9:16")]).await.unwrap()), "1152x2048");
    assert_eq!(size(&prep("glm-image", &[kv("ar", "4:3")]).await.unwrap()), "2048x1536");
    assert_eq!(size(&prep("glm-image", &[kv("size", "1K")]).await.unwrap()), "1024x1024");
    assert_eq!(size(&prep("glm-image", &[kv("size", "2K")]).await.unwrap()), "2048x2048");
    assert_eq!(size(&prep("cogview-4", &[kv("ar", "16:9")]).await.unwrap()), "1920x1072");
    assert_eq!(size(&prep("cogview-4", &[kv("ar", "3:4")]).await.unwrap()), "1248x1664");
    assert_eq!(size(&prep("cogview-3-flash", &[kv("size", "1K")]).await.unwrap()), "1024x1024");
    // Every size is within the documented rules.
    for (model, r) in [("glm-image", (21, 9)), ("cogview-4", (1, 4)), ("glm-image", (4, 1))] {
        let s = spec(model).unwrap().sizes;
        let (w, h) = size_for(&s, s.max_pixels, r).unwrap();
        assert!(w % s.multiple == 0 && h % s.multiple == 0 && (w as u64) * (h as u64) <= s.max_pixels);
        assert!(w >= s.min_side && h >= s.min_side && w <= s.max_side && h <= s.max_side);
    }
    let e = prep("cogview-4", &[kv("size", "2K")]).await.unwrap_err().message;
    assert!(e.contains("sizes: 1K"), "{e}");
    assert!(prep("glm-image", &[kv("size", "4K")]).await.unwrap_err().message.contains("sizes: 1K, 2K"));
    assert!(prep("glm-image", &[kv("ar", "8:1")]).await.unwrap_err().message.contains("has no size"));
    let p = prep("cogview-4", &[kv("q", "standard")]).await.unwrap();
    assert_eq!(p.body["quality"], "standard");
    assert_eq!(p.applied[0].source, "option");
    let e = prep("glm-image", &[kv("q", "standard")]).await.unwrap_err().message;
    assert!(e.contains("qualities: hd"), "{e}");
    for (opt, what) in [(kv("n", "2"), "(`n=`)"), (kv("bg", "transparent"), "(`bg=`)"), (flag("s"), "web search")] {
        let e = prep("glm-image", &[opt]).await.unwrap_err().message;
        assert!(e.contains(what), "{e}");
    }
    assert!(prep("glm-image", &[flag("i")]).await.unwrap_err().message.contains("without `+i`"));
    assert!(prep("glm-image", &[kv("e", "high")]).await.is_err());
    // Not in the table: its price is unknown to the bot, so it is not supported, with or without options.
    for opts in [&[][..], &[kv("ar", "16:9")]] {
        assert_eq!(
            prep("cogview-5", opts).await.unwrap_err().message,
            "`cogview-5` is not in the bot's Zhipu BigModel model table (its price is unknown), so it is not supported"
        );
    }
}

#[tokio::test]
async fn text_only_and_no_conversation() {
    let image = Part::Image {
        blob: BlobRef::for_bytes(b"x", "image/png"),
        filename: None,
        generated: false,
        dims: None,
        thought: false,
    };
    let with_image = vec![Turn {
        role: Role::User,
        origin: Origin::Live,
        parts: vec![Part::text("改"), image.clone()],
        native: None,
    }];
    assert!(prep_h("glm-image", &[], &with_image).await.unwrap_err().message.contains("text only"));
    let answer = Turn {
        role: Role::Assistant,
        origin: Origin::Live,
        parts: vec![Part::Image {
            blob: BlobRef::for_bytes(b"y", "image/png"),
            filename: None,
            generated: true,
            dims: None,
            thought: false,
        }],
        native: None,
    };
    // A reply to an image: it cannot be edited (no input image), which is refused.
    let reply = vec![Turn::user_text("画猫"), answer, Turn::user_text("改成蓝色")];
    let e = prep_h("glm-image", &[], &reply).await.unwrap_err();
    assert_eq!(
        e.message,
        "`glm-image` takes no input image, so it cannot edit the image replied to: start a new message"
    );
    // One prompt per request: a conversation would be ignored, which is refused.
    let chat = vec![Turn::user_text("讲个故事"), Turn::assistant_text("从前"), Turn::user_text("画出来")];
    let e = prep_h("cogview-4", &[], &chat).await.unwrap_err();
    assert_eq!(e.message, "`cogview-4` takes one prompt, not a conversation: start a new message");
    // A new message: its text only, nothing to say.
    let p = prep_h("cogview-4", &[], &[Turn::user_text("画出来")]).await.unwrap();
    assert_eq!(p.body["prompt"], "画出来");
    assert!(p.notes.is_empty() && p.warnings.is_empty(), "{:?} {:?}", p.notes, p.warnings);
    let long = "猫".repeat(1001);
    assert!(prep_h("glm-image", &[], &[Turn::user_text(long.as_str())]).await.unwrap_err().message.contains("1000"));
    let system = vec![
        Turn { role: Role::System, origin: Origin::Synthetic, parts: vec![Part::text("s")], native: None },
        Turn::user_text("x"),
    ];
    assert!(prep_h("glm-image", &[], &system).await.unwrap_err().message.contains("no system prompt"));
}

// ------------------------------------------------------------------ HTTP (local fake server)

/// Serve canned replies to successive connections (`{base}` in a body is the
/// server's own address); returns the address and the request heads seen.
async fn serve(replies: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (log, me) = (seen.clone(), addr.clone());
    tokio::spawn(async move {
        for (status, ctype, body) in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut buf, mut tmp) = (Vec::new(), vec![0u8; 65536]);
            let (head, sent) = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0usize);
                    while buf.len() < pos + 4 + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    break (head, serde_json::from_slice(&buf[pos + 4..pos + 4 + len]).unwrap_or(Value::Null));
                }
            };
            log.lock().unwrap().push((head, sent));
            let body = body.replace("{base}", &me);
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\nx-log-id: log-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(body.as_bytes()).await.unwrap();
            let _ = sock.shutdown().await;
        }
    });
    (addr, seen)
}

const PNG: &str = "\u{89}PNG";

fn png() -> String {
    // Bytes served as text: the magic is enough for the MIME sniffing.
    format!("{PNG}-image")
}

async fn run(z: &super::super::Zhipu, p: &Prepared) -> (Result<Value, LlmError>, CallOutcome, Vec<StreamEvent>) {
    let mut evs = Vec::new();
    let (r, o) = z.stream(p, &mut |e: StreamEvent| evs.push(e), &crate::NullObserver, &CancellationToken::new()).await;
    (r, o, evs)
}

#[tokio::test]
async fn glm_image_runs_as_a_resumable_task() {
    let (addr, seen) = serve(vec![
        (
            200,
            "application/json",
            json!({"model": "glm-image", "id": "task-9", "request_id": "r1", "task_status": "PROCESSING"}).to_string(),
        ),
        (
            200,
            "application/json",
            json!({"model": "glm-image", "task_status": "PROCESSING", "request_id": "r1"}).to_string(),
        ),
        (
            200,
            "application/json",
            json!({"model": "glm-image", "task_status": "SUCCESS", "request_id": "r1",
                                          "image_result": [{"url": "{base}/files/cat.png"}]})
            .to_string(),
        ),
        (200, "image/png", png()),
    ])
    .await;
    let z = zp_at(&format!("{addr}/api/paas/v4"));
    let p = z.prepare(&input("glm-image", &[], &[Turn::user_text("一只猫")]), &[], &NoBlobs).await.unwrap();
    let (r, outcome, evs) = run(&z, &p).await;
    let native = r.unwrap();
    assert_eq!(native["task_status"], "SUCCESS");
    assert_eq!(native["task"]["id"], "task-9");
    assert_eq!(outcome.upstream_id.as_deref(), Some("task-9"));
    // The task id is checkpointed before polling.
    let cp = evs.iter().position(|e| matches!(e, StreamEvent::Checkpoint(v) if v["task_id"] == "task-9")).unwrap();
    let img = evs.iter().position(|e| matches!(e, StreamEvent::Image { mime, .. } if mime == "image/png")).unwrap();
    assert!(cp < img);
    let usage = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
    assert!((usage.cost_usd.unwrap() - 0.1 / 7.0).abs() < 1e-12);
    assert!(evs.contains(&StreamEvent::Finish { reason: Some("stop".into()), native: None }));
    let heads: Vec<String> = seen.lock().unwrap().iter().map(|(h, _)| h.lines().next().unwrap().to_string()).collect();
    assert_eq!(
        heads,
        vec![
            "POST /api/paas/v4/async/images/generations HTTP/1.1",
            "GET /api/paas/v4/async-result/task-9 HTTP/1.1",
            "GET /api/paas/v4/async-result/task-9 HTTP/1.1",
            "GET /files/cat.png HTTP/1.1",
        ]
    );
    assert_eq!(seen.lock().unwrap()[0].1, p.body);
    assert!(seen.lock().unwrap()[1].0.to_ascii_lowercase().contains("authorization: bearer k"));

    // After a restart: the same request resumed from the checkpoint polls the task.
    let item = json!({"task_id": "task-9"});
    let hop = p.with_continuation(&item);
    assert_eq!(hop.url, format!("{addr}/api/paas/v4/async-result/task-9"));
    let (addr2, seen2) = serve(vec![
        (
            200,
            "application/json",
            json!({"model": "glm-image", "task_status": "SUCCESS", "image_result": [{"url": "{base}/x.png"}]})
                .to_string(),
        ),
        (200, "image/png", png()),
    ])
    .await;
    let z2 = zp_at(&format!("{addr2}/api/paas/v4"));
    let hop2 = zp_at(&format!("{addr2}/api/paas/v4"))
        .prepare(&input("glm-image", &[], &[Turn::user_text("一只猫")]), &[], &NoBlobs)
        .await
        .unwrap()
        .with_continuation(&item);
    let (r, _, evs) = run(&z2, &hop2).await;
    assert!(r.is_ok());
    assert!(!evs.iter().any(|e| matches!(e, StreamEvent::Checkpoint(_))), "no new task");
    assert_eq!(seen2.lock().unwrap()[0].0.lines().next().unwrap(), "GET /api/paas/v4/async-result/task-9 HTTP/1.1");
    assert_eq!(evs.iter().filter(|e| matches!(e, StreamEvent::Usage(_))).count(), 1, "billed once");
}

#[tokio::test]
async fn cogview_answers_directly_and_failures_are_verbatim() {
    let (addr, seen) = serve(vec![
        (200, "application/json", json!({"created": 1760335349, "data": [{"url": "{base}/c.png"}],
                                          "content_filter": [{"role": "assistant", "level": 3}]}).to_string()),
        (200, "image/png", png()),
        (400, "application/json", r#"{"contentFilter":[{"level":1,"role":"user"}],"error":{"code":"1301","message":"系统检测到输入或生成内容可能包含不安全或敏感内容，请您避免输入易产生敏感内容的提示语，感谢您的配合。"}}"#.into()),
        (200, "application/json", json!({"model": "glm-image", "id": "t2", "task_status": "PROCESSING"}).to_string()),
        (200, "application/json", json!({"model": "glm-image", "task_status": "FAIL", "request_id": "r"}).to_string()),
    ])
    .await;
    let z = zp_at(&format!("{addr}/api/paas/v4"));
    let p = z.prepare(&input("cogview-4-250304", &[], &[Turn::user_text("猫")]), &[], &NoBlobs).await.unwrap();
    let (r, outcome, evs) = run(&z, &p).await;
    assert_eq!(r.unwrap()["content_filter"][0]["level"], 3, "kept for the replay payload");
    assert_eq!(outcome.upstream_id, None);
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Meta { upstream_id: Some(id), .. } if id == "log-1")));
    assert!(evs.iter().any(|e| matches!(e, StreamEvent::Image { .. })));
    let usage = evs.iter().find_map(|e| if let StreamEvent::Usage(u) = e { Some(u) } else { None }).unwrap();
    assert!((usage.cost_usd.unwrap() - 0.06 / 7.0).abs() < 1e-12);
    assert_eq!(seen.lock().unwrap()[0].0.lines().next().unwrap(), "POST /api/paas/v4/images/generations HTTP/1.1");

    // Content safety: never retried, the body verbatim.
    let (r, _, _) = run(&z, &p).await;
    let e = r.unwrap_err();
    assert_eq!((e.status, e.retry, e.error_type.as_deref()), (Some(400), Retry::Never, Some("1301")));
    assert_eq!(e.details.unwrap()["contentFilter"][0]["level"], 1);

    // A task that failed without a reason (seen live): retried, so that the next attempt makes a new task.
    let p = z.prepare(&input("glm-image", &[], &[Turn::user_text("猫")]), &[], &NoBlobs).await.unwrap();
    let (r, _, _) = run(&z, &p).await;
    let e = r.unwrap_err();
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Backoff, Some("FAIL")));
    assert_eq!(e.details.unwrap()["task_status"], "FAIL");
}

/// A `FAIL` with a reason is classified by it.
#[test]
fn failed_tasks_with_a_reason() {
    let e = task_failed(
        json!({"model": "glm-image", "task_status": "FAIL", "request_id": "r",
        "error": {"code": "1301", "message": "系统检测到输入或生成内容可能包含不安全或敏感内容"}}),
        "image",
    );
    assert_eq!((e.kind, e.retry, e.error_type.as_deref()), (ErrorKind::InBand, Retry::Never, Some("1301")));
    assert_eq!(e.details.unwrap()["request_id"], "r", "the whole result, verbatim");
    let e = task_failed(
        json!({"model": "glm-image", "task_status": "FAIL", "error": {"code": "1234", "message": "网络错误"}}),
        "image",
    );
    assert_eq!(e.retry, Retry::Backoff);
    let e = task_failed(json!({"task_status": "FAIL", "content_filter": [{"role": "assistant", "level": 1}]}), "image");
    assert_eq!(e.retry, Retry::Never, "a content filter is never retried");
    let e = task_failed(json!({"task_status": "FAIL"}), "image");
    assert_eq!(e.retry, Retry::Backoff);
}
