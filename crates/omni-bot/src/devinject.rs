//! Simulated user messages for end-to-end tests (`[dev] inject_dir`).
//!
//! Bots cannot see messages from other bots, and tests should not need a user
//! account. A JSON file dropped into the inject directory is posted by the bot
//! as a clearly labelled "simulated message" (so replies have a real message
//! to attach to) and then recorded as an inbound message from `user_id` and
//! handled exactly like a real one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use omni_markdown::rich::{RBlock, RText};
use rusqlite::params;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::app::App;
use crate::conv::InboundPayload;
use crate::store::now_ms;
use crate::tg::outbox::{self, Content, Link, SendReq};

#[derive(Debug, Deserialize)]
struct Injected {
    #[serde(default)]
    bot: Option<String>,
    chat_id: i64,
    #[serde(default)]
    user_id: i64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    reply_to: Option<i32>,
    /// Instead of injecting: write these messages (as Telegram returns them)
    /// to `<inject_dir>/out/<file name>.txt`, to check what was rendered.
    #[serde(default)]
    dump: Vec<i32>,
    /// Instead of injecting: save the files and photos of these messages to
    /// `<inject_dir>/out/<message id>-<file name>`, to read a file someone
    /// posted or forwarded to the chat (at most 64 MiB each).
    #[serde(default)]
    download: Vec<i32>,
    /// Instead of injecting: send each named block list as a rich message,
    /// record whether Telegram accepts it, delete what was sent, and write the
    /// results to `<inject_dir>/out/<file name>.txt`. To learn Telegram's rules.
    #[serde(default)]
    probe: Vec<(String, Vec<RBlock>)>,
    /// Like `probe`, with Markdown rendered by the answer renderer.
    #[serde(default)]
    probe_md: Vec<(String, String)>,
    /// Attach this local file: the bot uploads it (as the simulated message,
    /// with `text` as caption) and the inbound message carries its media, so
    /// the real download/dedupe/conversion path runs.
    #[serde(default)]
    file: Option<PathBuf>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    as_photo: bool,
    /// Probe: keep the sent messages (to read them back) instead of deleting them.
    #[serde(default)]
    keep: bool,
    /// Post these local images as one album (the first item carries the
    /// caption) and record its items as Telegram delivers them: only the
    /// captioned item of a new conversation, every item of a reply to an answer.
    #[serde(default)]
    album: Vec<PathBuf>,
    /// Post a location message (latitude, longitude; no text) instead.
    #[serde(default)]
    location: Option<(f64, f64)>,
    /// Post `text` as a rich message (Markdown rendered like an answer) and
    /// record what Telegram stored, read back and converted like any incoming
    /// rich message.
    #[serde(default)]
    rich: bool,
    /// Instead of injecting: measure Telegram's edit rate limit. Each series sends
    /// one reply-like rich message and edits it through `sizes` (characters of
    /// its reasoning-like body), with the same gate and single-attempt calls as
    /// real replies. Results go to `<inject_dir>/out/<file name>.txt`.
    #[serde(default)]
    edit_series: Vec<EditSeries>,
    /// Instead of injecting: run timed sends/edits on several messages (no chat
    /// gate, exact schedule) to measure Telegram's rate limits precisely.
    /// Results go to `<inject_dir>/out/<file name>.txt`.
    #[serde(default)]
    rate_tests: Vec<RateTest>,
}

#[derive(Debug, Deserialize)]
struct RateTest {
    label: String,
    steps: Vec<RateStep>,
    /// Pause after the test (after deleting its messages).
    #[serde(default)]
    pause_after_secs: f64,
    /// Start the test at this millisecond of a wall-clock second (so that step
    /// times fall at known positions within server seconds).
    #[serde(default)]
    align_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RateStep {
    /// Message name within the test; the first `send` to a slot creates it.
    slot: String,
    /// `send`, `edit` or `delete` (single attempt; the slot is forgotten).
    op: String,
    chars: usize,
    /// `mixed` (default), `ascii` or `cjk` text.
    #[serde(default)]
    script: Option<String>,
    /// Start time, seconds after the test start (late steps run at once).
    at: f64,
    /// On FLOOD_WAIT, wait it out and try again (e.g. a setup send).
    #[serde(default)]
    retry: bool,
    /// Send/edit exactly these blocks instead of a generated text.
    #[serde(default)]
    blocks: Option<Vec<RBlock>>,
}

#[derive(Debug, Deserialize)]
struct EditSeries {
    label: String,
    /// Body size of the first message and of each following edit.
    sizes: Vec<usize>,
    /// Pause after the series, so that series do not affect each other.
    #[serde(default)]
    pause_after_secs: u64,
}

/// A reply-like rich message whose reasoning block holds `chars` characters.
pub(crate) fn series_content(label: &str, n: usize, chars: usize) -> Content {
    script_content(label, n, chars, "mixed")
}

fn script_unit(script: &str) -> &'static str {
    match script {
        "ascii" => "This text is used to probe Telegram's edit rate limit precisely. ",
        "cjk" => "这是一段用于精确测量电报编辑频率限制的中文文字内容。",
        // Texts sharing nothing with the ones above (an edit that rewrites everything).
        "ascii2" => "Another sentence with other words: flood control measured here! ",
        "cjk2" => "另外一句完全不同的话，专门拿来替换前面那段测试文本！",
        "mixed2" => "换一段完全不同的内容再试，Completely other words replace the old body now. ",
        // Astral characters: one code point, two UTF-16 units each.
        "emoji" => "😀🎉🚀🌟",
        _ => "这是一段用于测试编辑限速的文字，It is used to probe Telegram's edit rate limit. ",
    }
}

pub(crate) fn script_content(label: &str, n: usize, chars: usize, script: &str) -> Content {
    let unit = script_unit(script);
    let text: String = unit.chars().cycle().take(chars).collect();
    // Probe layouts, `<layout><K>-<script>`: exactly `chars` code points of body text in K even
    // paragraphs, whose last code point varies with `n` (so consecutive versions differ):
    //   plain (K=1) / paraK: the paragraphs alone;
    //   detK: inside a details block; hdetK: header, details, footer (like a reply);
    //   hparaK: header, the paragraphs, footer (no details);
    //   leadK: K paragraphs, the last K-1 of 100 code points each, the first holding the rest.
    let layouts = ["plain", "hpara", "hdet", "para", "det", "lead"];
    let split = layouts.iter().find_map(|l| {
        let rest = script.strip_prefix(l)?;
        let (k, script) = rest.split_once('-')?;
        let k =
            if *l == "plain" { (k.is_empty()).then_some(1)? } else { k.parse::<usize>().ok().filter(|k| *k > 0)? };
        Some((*l, k, script))
    });
    if let Some((layout, k, script)) = split {
        let mut text: Vec<char> = script_unit(script).chars().cycle().take(chars).collect();
        if let Some(last) = text.last_mut() {
            *last = char::from(b'0' + (n % 10) as u8);
        }
        let para = |c: &[char]| RBlock::Paragraph(RText::plain(c.iter().collect::<String>()));
        let paras: Vec<RBlock> = if layout == "lead" {
            let first = text.len().saturating_sub(100 * (k - 1));
            std::iter::once(para(&text[..first])).chain(text[first..].chunks(100).map(para)).collect()
        } else {
            text.chunks(text.len().div_ceil(k).max(1)).map(para).collect()
        };
        let header = || RBlock::Paragraph(RText::plain(format!("🤖 edit-rate test · {label}")));
        let footer = || RBlock::Footer(RText::plain(format!("edit {n} · {chars} chars")));
        let details = |blocks| RBlock::Details { open: true, title: RText::plain("💭 Reasoning"), blocks };
        return Content::Rich(match layout {
            "det" => vec![details(paras)],
            "hdet" => vec![header(), details(paras), footer()],
            "hpara" => [vec![header()], paras, vec![footer()]].concat(),
            _ => paras,
        });
    }
    let paragraphs: Vec<RBlock> = text
        .chars()
        .collect::<Vec<_>>()
        .chunks(2000)
        .map(|c| RBlock::Paragraph(RText::plain(c.iter().collect::<String>())))
        .collect();
    Content::Rich(vec![
        RBlock::Paragraph(RText::plain(format!("🤖 edit-rate test · {label}"))),
        RBlock::Details { open: true, title: RText::plain("💭 Reasoning"), blocks: paragraphs },
        RBlock::Footer(RText::plain(format!("edit {n} · {chars} chars"))),
    ])
}

/// Run rate tests one after another, writing the report after each test.
async fn run_rate_tests(
    app: &Arc<App>,
    bot: &Arc<crate::tg::bot::BotConn>,
    chat_id: i64,
    tests: &[RateTest],
    out: &Path,
) -> Result<()> {
    use crate::tg::outbox::OpError;
    let mut report = String::new();
    // A private gate without a budget: the schedule is exact (these probes measure
    // Telegram's limits; the chat's real gate is bypassed).
    let gate = crate::tg::gate::ChatGate::unlimited();
    for t in tests {
        if let Some(ms) = t.align_ms {
            let now = now_ms() as u64;
            let wait = (ms % 1000 + 1000 - now % 1000) % 1000;
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
        let t0 = tokio::time::Instant::now();
        let mut slots: std::collections::BTreeMap<String, i32> = Default::default();
        report.push_str(&format!("== {}\n", t.label));
        for (n, st) in t.steps.iter().enumerate() {
            tokio::time::sleep_until(t0 + Duration::from_secs_f64(st.at)).await;
            let script = if st.blocks.is_some() { "blocks" } else { st.script.as_deref().unwrap_or("mixed") };
            let content = match &st.blocks {
                Some(b) => Content::Rich(b.clone()),
                None => script_content(&format!("{} · {}", t.label, st.slot), n, st.chars, script),
            };
            let top = match &content {
                Content::Rich(b) => b.len(),
                Content::Text(_) => 1,
            };
            // The whole text as the server sees it: UTF-8 bytes and code points.
            let (bytes, cps) = match &content {
                Content::Rich(b) => {
                    let t = crate::tg::rich::expected_text(b);
                    (t.len(), t.chars().count())
                }
                Content::Text(f) => (f.text.len(), f.text.chars().count()),
            };
            loop {
                let permit = gate.acquire(crate::tg::gate::Ask::finals(crate::tg::flood::Weight::Light)).await;
                let wall_ms = now_ms();
                let start = t0.elapsed();
                let begun = std::time::Instant::now();
                let mut unchanged = None;
                let res = match (st.op.as_str(), slots.get(&st.slot)) {
                    ("send", _) => {
                        let link = Link {
                            turn_id: -4,
                            attempt_id: None,
                            part_index: (now_ms() & 0x3fff_ffff_ffff) * 64 + (n as i64 % 64),
                        };
                        let req = SendReq { chat_id, reply_to: None, content: &content, link, media: None };
                        outbox::send_once(&app.db, bot, &permit, &req, None).await.map(Some)
                    }
                    ("edit", Some(id)) => {
                        match outbox::edit_once_status(&app.db, bot, &permit, chat_id, *id, &content, None, None).await
                        {
                            Ok(outbox::Edited::Changed) => Ok(None),
                            Ok(other) => {
                                unchanged = Some(other);
                                Ok(None)
                            }
                            Err(e) => Err(e),
                        }
                    }
                    ("delete", Some(id)) => {
                        outbox::delete_once(&app.db, bot, &permit, chat_id, &[*id]).await.map(|_| None)
                    }
                    (op, _) => Err(OpError::Other(anyhow!("bad step {op} on {}", st.slot))),
                };
                let took = begun.elapsed();
                drop(permit);
                let verdict = match &res {
                    Ok(Some(id)) => {
                        slots.insert(st.slot.clone(), *id);
                        "ok".to_string()
                    }
                    Ok(None) => {
                        if st.op == "delete" {
                            slots.remove(&st.slot);
                        }
                        match unchanged {
                            Some(outbox::Edited::Unchanged) => "ok NOT_MODIFIED".to_string(),
                            Some(outbox::Edited::Gone) => "ok GONE".to_string(),
                            _ => "ok".to_string(),
                        }
                    }
                    Err(OpError::Flood(w)) => format!("FLOOD_WAIT {}", w.as_secs().saturating_sub(1)),
                    Err(e) => format!("ERROR {e}"),
                };
                report.push_str(&format!(
                "  #{n:<2} {:<4} {:<6} {:>6} chars {:>6} B {:>6} cp {top:>2} top {:<6} at {:>6.2}s start {:>7.3}s wall {wall_ms} took {:>4}ms  {verdict}\n",
                st.slot,
                st.op,
                st.chars,
                bytes,
                cps,
                script,
                st.at,
                start.as_secs_f64(),
                took.as_millis()
            ));
                if !(st.retry && matches!(res, Err(OpError::Flood(_)))) {
                    break;
                }
            }
        }
        // What Telegram stored, read back once after the timed steps.
        for (slot, id) in &slots {
            let line = match bot.get_messages(chat_id, &[*id]).await {
                Ok(msgs) => match msgs.first() {
                    Some(grammers_tl_types::enums::Message::Message(m)) => match &m.rich_message {
                        Some(grammers_tl_types::enums::RichMessage::Message(rm)) => {
                            let got = crate::tg::rich::received_text(&rm.blocks);
                            let tail: String =
                                got.chars().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect();
                            format!(
                                "{} cp stored, {} top-level blocks, ends {tail:?}{}",
                                got.chars().filter(|c| *c != '\u{1}').count(),
                                rm.blocks.len(),
                                if rm.part { ", part=true" } else { "" }
                            )
                        }
                        None => format!("not rich: {} chars", m.message.chars().count()),
                    },
                    other => format!("unexpected {other:?}").chars().take(80).collect(),
                },
                Err(e) => format!("read failed: {e}"),
            };
            report.push_str(&format!("  read {slot:<4} {line}\n"));
        }
        let ids: Vec<i32> = slots.values().copied().collect();
        let _ = outbox::delete(&app.db, bot, chat_id, &ids).await;
        report.push('\n');
        std::fs::write(out, &report)?;
        info!(label = %t.label, "dev: rate test done");
        tokio::time::sleep(Duration::from_secs_f64(t.pause_after_secs)).await;
    }
    std::fs::write(out, format!("{report}DONE\n"))?;
    Ok(())
}

/// Run edit series one after another; returns the report.
async fn run_edit_series(
    app: &Arc<App>,
    bot: &Arc<crate::tg::bot::BotConn>,
    chat_id: i64,
    series: &[EditSeries],
    keep: bool,
) -> String {
    use crate::tg::flood::{self, Shape};
    use crate::tg::gate::{Ask, Class};
    use crate::tg::outbox::OpError;
    let mut report = String::new();
    let gate = bot.gates.get(chat_id);
    let mut shown: Option<Shape> = None;
    for s in series {
        let t0 = std::time::Instant::now();
        let mut msg: Option<i32> = None;
        let (mut floods, mut flood_secs) = (0u32, 0u64);
        let link = Link { turn_id: -3, attempt_id: None, part_index: (now_ms() & 0x3fff_ffff_ffff) };
        let mut n = 0usize;
        while n < s.sizes.len() {
            let content = series_content(&s.label, n, s.sizes[n]);
            // Through the chat's real gate, like a streaming reply.
            let weight = match msg {
                None => flood::send_weight(Shape::of(&content)),
                Some(_) => flood::edit_weight(shown, Shape::of(&content)),
            };
            let permit = gate.acquire(Ask { class: Class::Stream, weight, not_before: None }).await;
            permit.set_weight(weight);
            let start = t0.elapsed();
            let begun = std::time::Instant::now();
            let res = match msg {
                None => outbox::send_once(
                    &app.db,
                    bot,
                    &permit,
                    &SendReq { chat_id, reply_to: None, content: &content, link, media: None },
                    None,
                )
                .await
                .map(Some),
                Some(id) => {
                    outbox::edit_once(&app.db, bot, &permit, chat_id, id, &content, None, None).await.map(|_| None)
                }
            };
            let took = begun.elapsed();
            drop(permit);
            let line = match &res {
                Ok(_) => "ok".to_string(),
                Err(OpError::Flood(w)) => {
                    floods += 1;
                    flood_secs += w.as_secs();
                    format!("FLOOD_WAIT {}s", w.as_secs().saturating_sub(1))
                }
                Err(e) => format!("ERROR {e}"),
            };
            report.push_str(&format!(
                "{:<10} op {:>2} size {:>6}  t={:>6.1}s  took {:>4}ms  {line}\n",
                s.label,
                n,
                s.sizes[n],
                start.as_secs_f64(),
                took.as_millis()
            ));
            match res {
                Ok(Some(id)) => {
                    msg = Some(id);
                    shown = Some(Shape::of(&content));
                    n += 1;
                }
                Ok(None) => {
                    shown = Some(Shape::of(&content));
                    n += 1;
                }
                Err(OpError::Flood(_)) => {} // the gate waits it out; the same edit again
                Err(_) => break,
            }
        }
        report.push_str(&format!(
            "{:<10} SUMMARY {} ops in {:.1}s, {floods} flood waits ({flood_secs}s)\n\n",
            s.label,
            s.sizes.len(),
            t0.elapsed().as_secs_f64()
        ));
        info!(label = %s.label, floods, "dev: edit series done");
        if let Some(id) = msg
            && !keep
        {
            let _ = outbox::delete(&app.db, bot, chat_id, &[id]).await;
        }
        tokio::time::sleep(Duration::from_secs(s.pause_after_secs)).await;
    }
    report
}

pub fn spawn(app: Arc<App>, dir: PathBuf) {
    tokio::spawn(async move {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), error = %e, "cannot create inject dir");
            return;
        }
        warn!(dir = %dir.display(), "dev: simulated user messages are enabled");
        loop {
            let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
            files.retain(|p| p.extension().is_some_and(|x| x == "json"));
            files.sort();
            for f in files {
                match inject(&app, &f).await {
                    Ok(msg_id) => info!(file = %f.display(), msg_id, "dev: injected simulated message"),
                    Err(e) => warn!(file = %f.display(), error = %format!("{e:#}"), "dev: injection failed"),
                }
                let _ = std::fs::remove_file(&f);
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = app.shutdown.cancelled() => return,
            }
        }
    });
}

async fn inject(app: &Arc<App>, path: &Path) -> Result<i32> {
    let raw = std::fs::read(path)?;
    let req: Injected = serde_json::from_slice(&raw).context("bad inject file")?;
    let bot = match &req.bot {
        Some(b) => app.bot(b).cloned().ok_or_else(|| anyhow!("no bot {b}"))?,
        None => app.bots.first().cloned().ok_or_else(|| anyhow!("no bots"))?,
    };
    let mut req = req;
    if !req.rate_tests.is_empty() {
        let out_dir = path.parent().unwrap_or(Path::new(".")).join("out");
        std::fs::create_dir_all(&out_dir)?;
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        run_rate_tests(app, &bot, req.chat_id, &req.rate_tests, &out_dir.join(format!("{name}.txt"))).await?;
        return Ok(0);
    }
    if !req.edit_series.is_empty() {
        let out_dir = path.parent().unwrap_or(Path::new(".")).join("out");
        std::fs::create_dir_all(&out_dir)?;
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let report = run_edit_series(app, &bot, req.chat_id, &req.edit_series, req.keep).await;
        std::fs::write(out_dir.join(format!("{name}.txt")), report)?;
        return Ok(0);
    }
    for (label, md) in std::mem::take(&mut req.probe_md) {
        let blocks = omni_markdown::rich::markdown_to_blocks(&md, 0, omni_markdown::rich::RichLimits::TELEGRAM);
        req.probe.push((label, omni_markdown::rich::sanitize(blocks)));
    }
    if !req.probe.is_empty() {
        let out_dir = path.parent().unwrap_or(Path::new(".")).join("out");
        std::fs::create_dir_all(&out_dir)?;
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mut report = String::new();
        let mut media = crate::tg::rich::RichMedia::default();
        for (i, (label, blocks)) in req.probe.iter().enumerate() {
            // Media keys are blob hashes (photos) or `hash/filename` (documents) of existing blobs.
            let mut sources = Vec::new();
            for key in omni_markdown::rich::media_keys(blocks) {
                let (sha, name) = match key.split_once('/') {
                    Some((h, n)) => (h.to_string(), Some(n.to_string())),
                    None => (key.clone(), None),
                };
                let sha2 = sha.clone();
                let meta: Option<(i64, Option<String>)> = app
                    .db
                    .call(move |c| {
                        Ok(rusqlite::OptionalExtension::optional(c.query_row(
                            "SELECT size, mime FROM blobs WHERE sha256 = ?1",
                            params![sha2],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        ))?)
                    })
                    .await?;
                let Some((size, mime)) = meta else { anyhow::bail!("no blob {sha}") };
                let blob = omni_core::BlobRef { sha256: sha, size: size as u64, mime: mime.unwrap_or_default() };
                sources.push(crate::tg::upload::Source {
                    key: key.clone(),
                    blob,
                    filename: name.clone().unwrap_or_else(|| "image.jpg".into()),
                    want: if name.is_some() {
                        crate::tg::upload::Want::Document
                    } else {
                        crate::tg::upload::Want::Photo
                    },
                });
            }
            crate::tg::upload::ensure(app, &bot, req.chat_id, &sources, &mut media).await?;
            let content = Content::Rich(blocks.clone());
            let h = Sha256::digest(format!("{name}/{i}").as_bytes());
            let tag = i64::from_be_bytes(h[..8].try_into().unwrap()) & 0x3fff_ffff_ffff_ffff;
            let link = Link { turn_id: -2, attempt_id: None, part_index: tag };
            let res = outbox::send(
                &app.db,
                &bot,
                SendReq { chat_id: req.chat_id, reply_to: None, content: &content, link, media: Some(&media) },
            )
            .await;
            match res {
                Ok(id) => {
                    // Read it back and compare the stored text with what was sent.
                    let verdict = match bot.get_messages(req.chat_id, &[id]).await {
                        Ok(msgs) => match msgs.first() {
                            Some(grammers_tl_types::enums::Message::Message(m)) => match &m.rich_message {
                                Some(grammers_tl_types::enums::RichMessage::Message(rm)) => {
                                    let got = crate::tg::rich::received_text(&rm.blocks);
                                    let want = crate::tg::rich::expected_text(blocks);
                                    let part = if rm.part { " part=true" } else { "" };
                                    if got == want {
                                        format!("intact ({} chars){part}", want.chars().count())
                                    } else {
                                        let at = got.chars().zip(want.chars()).take_while(|(a, b)| a == b).count();
                                        let around = |s: &str| {
                                            s.chars().skip(at.saturating_sub(10)).take(30).collect::<String>()
                                        };
                                        format!(
                                            "CHANGED{part}: sent {} chars, stored {} chars, first difference at char {at}: sent {:?} stored {:?}",
                                            want.chars().count(),
                                            got.chars().count(),
                                            around(&want),
                                            around(&got)
                                        )
                                    }
                                }
                                None => format!(
                                    "CHANGED: no rich message stored (text {:?})",
                                    m.message.chars().take(40).collect::<String>()
                                ),
                            },
                            other => format!("CHANGED: unexpected {other:?}"),
                        },
                        Err(e) => format!("read-back failed: {e}"),
                    };
                    report.push_str(&format!("OK   {label} msg={id} · {verdict}\n"));
                    if !req.keep {
                        let _ = outbox::delete(&app.db, &bot, req.chat_id, &[id]).await;
                    }
                }
                Err(e) => report.push_str(&format!("FAIL {label}: {e}\n")),
            }
        }
        std::fs::write(out_dir.join(format!("{name}.txt")), report)?;
        return Ok(0);
    }
    if !req.dump.is_empty() {
        let msgs = bot.get_messages(req.chat_id, &req.dump).await?;
        let out_dir = path.parent().unwrap_or(Path::new(".")).join("out");
        std::fs::create_dir_all(&out_dir)?;
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let text: String = msgs.iter().map(|m| format!("{m:#?}\n")).collect();
        std::fs::write(out_dir.join(format!("{name}.txt")), text)?;
        return Ok(0);
    }
    if !req.download.is_empty() {
        let msgs = bot.get_messages(req.chat_id, &req.download).await?;
        let out_dir = path.parent().unwrap_or(Path::new(".")).join("out");
        std::fs::create_dir_all(&out_dir)?;
        for m in &msgs {
            let grammers_tl_types::enums::Message::Message(m) = m else { continue };
            for r in crate::media::media_of(m) {
                let bytes = crate::media::download(&bot, &r, 64 << 20).await?;
                let fname = r.filename.clone().unwrap_or_else(|| "file".into());
                let safe: String =
                    fname.chars().map(|c| if c.is_alphanumeric() || ".-_".contains(c) { c } else { '_' }).collect();
                std::fs::write(out_dir.join(format!("{}-{safe}", m.id)), bytes)?;
            }
        }
        return Ok(0);
    }
    if !req.album.is_empty() || req.location.is_some() || req.rich {
        return inject_media(app, &bot, &req, &raw).await;
    }
    let content = Content::Rich(vec![
        RBlock::Paragraph(RText::concat(vec![
            RText::plain("🧪 "),
            RText::bold(RText::plain("simulated message")),
            RText::plain(format!(" from user {}:", req.user_id)),
        ])),
        RBlock::Pre { text: req.text.clone(), language: String::new() },
    ]);
    let h = Sha256::digest(&raw);
    let tag = i64::from_be_bytes(h[..8].try_into().unwrap()) & 0x3fff_ffff_ffff_ffff;
    let link = Link { turn_id: -1, attempt_id: None, part_index: tag };
    let mut payload = InboundPayload { sender_name: Some("simulated".into()), ..Default::default() };
    let msg_id = match &req.file {
        None => outbox::send(
            &app.db,
            &bot,
            SendReq { chat_id: req.chat_id, reply_to: req.reply_to, content: &content, link, media: None },
        )
        .await
        .map_err(|e| anyhow!("send: {e}"))?,
        Some(f) => {
            let bytes = std::fs::read(f).with_context(|| format!("read {}", f.display()))?;
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
            let mime = req.mime.clone().unwrap_or_else(|| "application/octet-stream".into());
            let caption = format!("🧪 simulated message from user {}:\n{}", req.user_id, req.text);
            let id = outbox::send_file(
                &app.db,
                &bot,
                req.chat_id,
                req.reply_to,
                bytes,
                &name,
                &mime,
                req.as_photo,
                &caption,
                link,
            )
            .await
            .map_err(|e| anyhow!("send file: {e}"))?;
            // Read the message back to get its media exactly as an incoming update would.
            let msgs = bot.get_messages(req.chat_id, &[id]).await?;
            if let Some(grammers_tl_types::enums::Message::Message(m)) = msgs.first() {
                payload.media = crate::media::media_of(m);
            }
            id
        }
    };
    let payload = serde_json::to_string(&payload)?;
    let (b, chat, user, text, reply_to) = (bot.name.clone(), req.chat_id, req.user_id, req.text.clone(), req.reply_to);
    let id: Option<i64> = app
        .db
        .call(move |c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO inbound(bot, chat_id, msg_id, sender_id, date, text, reply_to_msg_id, grouped_id, payload, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, 'new', ?9)",
                params![b, chat, msg_id, user, now_ms() / 1000, text, reply_to, payload, now_ms()],
            )?;
            Ok((n > 0).then(|| c.last_insert_rowid()))
        })
        .await?;
    if let Some(id) = id {
        crate::handler::dispatch(app.clone(), id);
    }
    Ok(msg_id)
}

/// Simulated albums, locations and rich messages: posted by the bot, read back,
/// and recorded as Telegram delivers them (an album as one message per item).
async fn inject_media(app: &Arc<App>, bot: &Arc<crate::tg::bot::BotConn>, req: &Injected, raw: &[u8]) -> Result<i32> {
    use grammers_tl_types as tl;
    let peer = bot.input_peer(req.chat_id).await?;
    let label = format!("🧪 simulated message from user {}:\n{}", req.user_id, req.text);
    let random = || -> i64 { rand::random::<i64>() };
    let ids: Vec<i32> = if !req.album.is_empty() {
        let mut items = Vec::new();
        for (i, f) in req.album.iter().enumerate() {
            let bytes = std::fs::read(f).with_context(|| format!("read {}", f.display()))?;
            let name = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "photo.jpg".into());
            let size = bytes.len();
            let uploaded = bot
                .client
                .upload_stream(&mut std::io::Cursor::new(bytes), size, name)
                .await
                .map_err(|e| anyhow!("upload: {e}"))?;
            let media = bot
                .invoke(&tl::functions::messages::UploadMedia {
                    business_connection_id: None,
                    peer: peer.clone(),
                    media: tl::types::InputMediaUploadedPhoto {
                        spoiler: false,
                        live_photo: false,
                        file: uploaded.raw.clone(),
                        stickers: None,
                        ttl_seconds: None,
                        video: None,
                    }
                    .into(),
                })
                .await
                .map_err(|e| anyhow!("uploadMedia: {e}"))?;
            let tl::enums::MessageMedia::Photo(tl::types::MessageMediaPhoto {
                photo: Some(tl::enums::Photo::Photo(photo)),
                ..
            }) = media
            else {
                anyhow::bail!("uploadMedia returned no photo");
            };
            let random_id = random();
            let single: tl::enums::InputSingleMedia = tl::types::InputSingleMedia {
                media: tl::types::InputMediaPhoto {
                    spoiler: false,
                    live_photo: false,
                    id: tl::types::InputPhoto {
                        id: photo.id,
                        access_hash: photo.access_hash,
                        file_reference: photo.file_reference.clone(),
                    }
                    .into(),
                    ttl_seconds: None,
                    video: None,
                }
                .into(),
                random_id,
                message: if i == 0 { label.clone() } else { String::new() },
                entities: None,
            }
            .into();
            items.push((random_id, single));
        }
        let updates = bot
            .invoke(&tl::functions::messages::SendMultiMedia {
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer: peer.clone(),
                reply_to: outbox::reply_header(req.reply_to),
                multi_media: items.iter().map(|(_, m)| m.clone()).collect(),
                schedule_date: None,
                send_as: None,
                quick_reply_shortcut: None,
                effect: None,
                allow_paid_stars: None,
            })
            .await
            .map_err(|e| anyhow!("sendMultiMedia: {e}"))?;
        items.iter().filter_map(|(r, _)| crate::tg::convert::sent_message_id(&updates, *r)).collect()
    } else if let Some((lat, long)) = req.location {
        let random_id = random();
        let updates = bot
            .invoke(&tl::functions::messages::SendMedia {
                silent: false,
                background: false,
                clear_draft: false,
                noforwards: false,
                update_stickersets_order: false,
                invert_media: false,
                allow_paid_floodskip: false,
                peer: peer.clone(),
                reply_to: outbox::reply_header(req.reply_to),
                media: tl::types::InputMediaGeoPoint {
                    geo_point: tl::types::InputGeoPoint { lat, long, accuracy_radius: None }.into(),
                }
                .into(),
                message: String::new(),
                random_id,
                reply_markup: None,
                entities: None,
                schedule_date: None,
                schedule_repeat_period: None,
                send_as: None,
                quick_reply_shortcut: None,
                effect: None,
                allow_paid_stars: None,
                suggested_post: None,
            })
            .await
            .map_err(|e| anyhow!("sendMedia: {e}"))?;
        crate::tg::convert::sent_message_id(&updates, random_id).into_iter().collect()
    } else {
        // A labelled rich message: the label paragraph, then the content.
        let mut blocks = vec![RBlock::Paragraph(RText::concat(vec![
            RText::plain("🧪 "),
            RText::bold(RText::plain("simulated rich message")),
            RText::plain(format!(" from user {}:", req.user_id)),
        ]))];
        blocks.extend(omni_markdown::rich::sanitize(omni_markdown::rich::markdown_to_blocks(
            &req.text,
            0,
            omni_markdown::rich::RichLimits::TELEGRAM,
        )));
        let h = Sha256::digest(raw);
        let tag = i64::from_be_bytes(h[..8].try_into().unwrap()) & 0x3fff_ffff_ffff_ffff;
        let link = Link { turn_id: -1, attempt_id: None, part_index: tag };
        let content = Content::Rich(blocks);
        let id = outbox::send(
            &app.db,
            bot,
            SendReq { chat_id: req.chat_id, reply_to: req.reply_to, content: &content, link, media: None },
        )
        .await
        .map_err(|e| anyhow!("send: {e}"))?;
        vec![id]
    };
    let first = *ids.first().ok_or_else(|| anyhow!("nothing was sent"))?;
    let replying_to_answer = match req.reply_to {
        Some(rt) => crate::conv::turn_for_message(&app.db, &bot.name, req.chat_id, rt).await?.is_some(),
        None => false,
    };
    let msgs = bot.get_messages(req.chat_id, &ids).await?;
    for (i, m) in msgs.iter().enumerate() {
        let grammers_tl_types::enums::Message::Message(m) = m else { continue };
        // A new conversation's album: only its captioned item carries the prefix.
        if i > 0 && !replying_to_answer {
            continue;
        }
        let text = match &m.rich_message {
            Some(grammers_tl_types::enums::RichMessage::Message(rm)) if req.rich => {
                crate::tg::richin::convert(rm.blocks.get(1..).unwrap_or(&[])).markdown
            }
            _ if i == 0 => req.text.clone(),
            _ => String::new(),
        };
        let payload = InboundPayload {
            sender_name: Some("simulated".into()),
            media: crate::media::media_of(m),
            locations: crate::media::locations_of(m),
            rich: req.rich,
            ..Default::default()
        };
        let payload = serde_json::to_string(&payload)?;
        let (b, chat, user, reply_to, grouped, msg_id) =
            (bot.name.clone(), req.chat_id, req.user_id, req.reply_to, m.grouped_id, m.id);
        let id: Option<i64> = app
            .db
            .call(move |c| {
                let n = c.execute(
                    "INSERT OR IGNORE INTO inbound(bot, chat_id, msg_id, sender_id, date, text, reply_to_msg_id, grouped_id, payload, status, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'new', ?10)",
                    params![b, chat, msg_id, user, now_ms() / 1000, text, reply_to, grouped, payload, now_ms()],
                )?;
                Ok((n > 0).then(|| c.last_insert_rowid()))
            })
            .await?;
        if let Some(id) = id {
            info!(msg = msg_id, inbound = id, "recorded simulated media message");
            crate::handler::dispatch(app.clone(), id);
        }
    }
    Ok(first)
}
