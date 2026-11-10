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
