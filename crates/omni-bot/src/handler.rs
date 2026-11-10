//! Turning recorded inbound messages into conversation turns and jobs.
//!
//! Handlers are idempotent: if the turns of an inbound message already exist
//! (crash after creating them), only the jobs are (re)scheduled.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use omni_core::Turn;
use omni_core::content::{Origin, Part, Role};
use omni_core::trigger::{self, SpecToken};
use tracing::{Instrument, info, info_span, warn};

use crate::access::{self, Access};
use crate::app::App;
use crate::conv::{self, Inbound, NewTurn, ResolvedSpec, TurnStatus};
use crate::intake::{Kind, parse_command, reply_target};
use crate::jobs;
use crate::media;
use crate::reply;
use crate::tg::bot::BotConn;

pub fn dispatch(app: Arc<App>, inbound_id: i64) {
    tokio::spawn(async move {
        let span = info_span!("inbound", id = inbound_id);
        let res = handle(app.clone(), inbound_id).instrument(span).await;
        let (status, err) = match &res {
            Ok(()) => ("done", None),
            Err(e) => ("error", Some(format!("{e:#}"))),
        };
        if let Some(e) = &err {
            warn!(inbound = inbound_id, error = %e, "inbound handling failed");
        }
        let _ = conv::set_inbound_status(&app.db, inbound_id, status, err).await;
    });
}

/// Re-dispatch inbound messages that were recorded but not handled (restart).
pub async fn recover(app: Arc<App>) -> Result<()> {
    for id in conv::pending_inbound(&app.db).await? {
        dispatch(app.clone(), id);
    }
    Ok(())
}

async fn handle(app: Arc<App>, id: i64) -> Result<()> {
    let ib = conv::get_inbound(&app.db, id).await?;
    if ib.status != "new" {
        return Ok(());
    }
    let bot = app.bot(&ib.bot).cloned().ok_or_else(|| anyhow!("bot {} not configured", ib.bot))?;
    let span = info_span!("msg", bot = %ib.bot, chat = ib.chat_id, user = ib.sender_id.unwrap_or(0), msg = ib.msg_id);
    async move {
        let user = ib.sender_id.unwrap_or(0);
        // Existing turns => crashed after creating them: just (re)start jobs.
        let existing = conv::turns_of_inbound(&app.db, ib.id).await?;
        if !existing.is_empty() {
            for (t, role) in existing {
                if role == Role::Assistant {
                    jobs::spawn_turn(app.clone(), t);
                }
            }
            return Ok(());
        }
        let kind = kind_of(&app, &bot, &ib).await;
        // Commands handle their own permission checks (e.g. /whoami, /ping work for everyone).
        if let Some(Kind::Command(name)) = &kind {
            let args = parse_command(&ib.text, &bot.username).map(|(_, a, _)| a.to_string()).unwrap_or_default();
            return crate::commands::run(&app, &bot, &ib, name, &args).await;
        }
        match access::check(&app, ib.chat_id, user).await? {
            Access::Allowed => {}
            Access::Denied(msg) => {
                reply::notice(&app, &bot, ib.chat_id, ib.msg_id, &format!("⛔ {msg}")).await;
                return Ok(());
            }
            Access::Silent => {
                info!("ignored (not allowed)");
                return Ok(());
            }
        }
        match kind {
            Some(Kind::Trigger) => new_conversation(&app, &bot, &ib).await,
            Some(Kind::Reply) => continue_conversation(&app, &bot, &ib, None).await,
            Some(Kind::Hint) => {
                reply::notice(&app, &bot, ib.chat_id, ib.msg_id, crate::commands::HINT).await;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    .instrument(span)
    .await
}

async fn kind_of(app: &App, bot: &BotConn, ib: &Inbound) -> Option<Kind> {
    if let Some((name, _, _)) = parse_command(&ib.text, &bot.username) {
        return Some(Kind::Command(name));
    }
    if let Some(rt) = ib.reply_to_msg_id
        && conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await.ok().flatten().is_some()
    {
        return Some(Kind::Reply);
    }
    if trigger::find_trigger_dollar(&ib.text).is_some() {
        return Some(Kind::Trigger);
    }
    if ib.chat_id > 0 {
        return Some(Kind::Hint);
    }
    None
}

/// Resolve the specs of a trigger that this bot is responsible for.
pub async fn resolve_specs(
    app: &App,
    bot: &BotConn,
    chat_id: i64,
    specs: &[SpecToken],
) -> Result<Vec<ResolvedSpec>, String> {
    let cfg = app.bot_cfg(&bot.name).ok_or("bot not configured")?;
    if specs.len() > app.cfg.general.max_parallel_specs {
        return Err(format!("too many models in one request (max {})", app.cfg.general.max_parallel_specs));
    }
    let strict = chat_id < 0;
    let mut out = Vec::new();
    let mut unknown = Vec::new();
    for s in specs {
        match app.resolve_spec(s, cfg, strict).await? {
            Some(spec) => {
                if app.responsible_bot(chat_id, &spec.canonical(), &bot.name).as_deref() == Some(bot.name.as_str()) {
                    out.push(spec);
                } else if chat_id > 0 {
                    return Err(format!("this bot does not serve `{}` (see /models)", spec.model));
                }
            }
            None => unknown.push(if s.raw.is_empty() { "$".to_string() } else { s.raw.clone() }),
        }
    }
    if !unknown.is_empty() && chat_id > 0 {
        return Err(format!(
            "unknown model or alias: {}. Use an alias from /models or a full OpenRouter model id like `openai/gpt-6$`",
            unknown.join(", ")
        ));
    }
    Ok(out)
}

/// Collect the media of a message (album-aware) as blobs/parts.
async fn media_parts(app: &App, bot: &BotConn, ib: &Inbound) -> Result<Vec<Part>> {
    let max = app.cfg.general_max_file_bytes();
    let mut refs = ib.payload.media.clone();
    if let Some(g) = ib.grouped_id {
        // Give the other album parts a moment to arrive, then fetch them by id.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        if let Ok(parts) = media::album(bot, ib.chat_id, ib.msg_id, g).await
            && !parts.is_empty()
        {
            refs = parts;
        }
    }
    // Media of a replied-to (non-bot) message, like the old bot.
    if let Some(rt) = ib.reply_to_msg_id
        && conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await?.is_none()
        && let Ok(msgs) = bot.get_messages(ib.chat_id, &[rt]).await
    {
        for m in msgs {
            if let grammers_tl_types::enums::Message::Message(m) = m {
                let mut r = media::media_of(&m);
                if let (Some(g), true) = (m.grouped_id, !r.is_empty())
                    && let Ok(parts) = media::album(bot, ib.chat_id, m.id, g).await
                    && !parts.is_empty()
                {
                    r = parts;
                }
                let mut combined = r;
                combined.extend(refs);
                refs = combined;
            }
        }
    }
    let mut parts = Vec::new();
    for r in refs.iter().filter(|r| !r.is_sticker) {
        let blob = media::fetch(&app.db, &app.blobs, bot, ib.chat_id, r, max)
            .await
            .with_context(|| format!("download {}", r.describe()))?;
        parts.push(match r.kind {
            media::MediaKind::Photo => Part::Image { blob, filename: None, generated: false },
            media::MediaKind::Document if r.mime.starts_with("image/") => {
                Part::Image { blob, filename: r.filename.clone(), generated: false }
            }
            media::MediaKind::Document => Part::File { blob, filename: r.filename.clone(), generated: false },
        });
    }
    Ok(parts)
}

/// Build user turns from message text (role markers -> synthetic history).
fn user_turns(text: &str, media: Vec<Part>) -> Result<Vec<Turn>, String> {
    let segments = trigger::parse_history(text);
    let mut turns: Vec<Turn> = match segments {
        None => vec![Turn {
            role: Role::User,
            origin: Origin::Live,
            parts: if text.is_empty() { vec![] } else { vec![Part::text(text)] },
            native: None,
        }],
        Some(segs) => segs
            .into_iter()
            .map(|s| Turn {
                role: s.role,
                origin: Origin::Synthetic,
                parts: if s.text.is_empty() { vec![] } else { vec![Part::text(s.text)] },
                native: None,
            })
            .collect(),
    };
    // Media belongs to the last user turn.
    if !media.is_empty() {
        let idx = turns
            .iter()
            .rposition(|t| t.role == Role::User)
            .ok_or("history has no user turn to attach the media to")?;
        let t = &mut turns[idx];
        let mut parts = media;
        parts.append(&mut t.parts);
        t.parts = parts;
        t.origin = Origin::Live;
    }
    if let Some(last_user) = turns.iter_mut().rev().find(|t| t.role == Role::User) {
        last_user.origin = Origin::Live;
        // `@https://youtube.com/...` attaches the video by URL (fetched by the provider, e.g. Gemini),
        // like the old bot. The text itself is sent unchanged.
        let urls: Vec<String> = last_user
            .parts
            .iter()
            .filter_map(|p| if let Part::Text { text, .. } = p { Some(youtube_refs(text)) } else { None })
            .flatten()
            .collect();
        for url in urls {
            last_user.parts.insert(0, Part::RemoteMedia { url, kind: omni_core::content::RemoteMediaKind::YouTube });
        }
    }
    if turns.iter().all(|t| t.parts.is_empty()) {
        return Err("empty message".into());
    }
    Ok(turns)
}

/// `@https://www.youtube.com/watch?v=...` / `@https://youtu.be/...` references in text.
pub fn youtube_refs(text: &str) -> Vec<String> {
    let re =
        regex::Regex::new(r"@(https://(?:(?:www\.|m\.)?youtube\.com|youtu\.be)/[A-Za-z0-9./?=&%_~#:+-]+)").unwrap();
    re.captures_iter(text).map(|c| c[1].to_string()).collect()
}

async fn new_conversation(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Ok(Some(t)) = trigger::parse_trigger(&ib.text).map_err(|e| e.to_string()).inspect_err(|e| {
        let e = e.clone();
        let (app, bot, chat, msg) = (app.clone(), bot.clone(), ib.chat_id, ib.msg_id);
        tokio::spawn(async move { reply::notice(&app, &bot, chat, msg, &format!("⚠️ {e}")).await });
    }) else {
        return Ok(());
    };
    let specs = match resolve_specs(app, bot, ib.chat_id, &t.specs).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => return Ok(()),
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    let media = match media_parts(app, bot, ib).await {
        Ok(m) => m,
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e:#}")).await;
            return Ok(());
        }
    };
    let turns = match user_turns(&t.text, media) {
        Ok(t) => t,
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    create_and_start(app, bot, ib, None, turns, specs).await
}

/// A reply to a bot message (or `/m` with new specs).
pub async fn continue_conversation(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    ib: &Inbound,
    new_specs: Option<(Vec<ResolvedSpec>, String)>,
) -> Result<()> {
    if ib.payload.has_quote && new_specs.is_none() {
        info!("reply with quote ignored");
        return Ok(());
    }
    let rt = ib.reply_to_msg_id.context("not a reply")?;
    let Some((turn_id, _)) = conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await? else {
        return Ok(());
    };
    let parent = conv::get_turn(&app.db, turn_id).await?;
    if parent.turn.role == Role::Assistant && parent.status == TurnStatus::Failed {
        reply::notice(
            app,
            bot,
            ib.chat_id,
            ib.msg_id,
            "⚠️ That message failed to generate, so there is nothing to continue from. Reply /retry to it to try again, or reply to an earlier message.",
        )
        .await;
        return Ok(());
    }
    let (specs, text) = match new_specs {
        Some(s) => s,
        None => {
            let spec = parent.spec.clone().context("replied turn has no model")?;
            // A leading backslash escapes commands/markers: everything after it is sent verbatim.
            let text = ib.text.strip_prefix('\\').unwrap_or(&ib.text).to_string();
            (vec![spec], text)
        }
    };
    let media = match media_parts(app, bot, ib).await {
        Ok(m) => m,
        Err(e) => {
            reply::notice(app, bot, ib.chat_id, ib.msg_id, &format!("⚠️ {e:#}")).await;
            return Ok(());
        }
    };
    let mut parts = media;
    if !text.is_empty() {
        parts.push(Part::text(text));
    }
    if parts.is_empty() {
        reply::notice(app, bot, ib.chat_id, ib.msg_id, "⚠️ empty message").await;
        return Ok(());
    }
    let turns = vec![Turn { role: Role::User, origin: Origin::Live, parts, native: None }];
    create_and_start(app, bot, ib, Some(turn_id), turns, specs).await
}

async fn create_and_start(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    ib: &Inbound,
    parent: Option<i64>,
    turns: Vec<Turn>,
    specs: Vec<ResolvedSpec>,
) -> Result<()> {
    let (bot_name, chat, user, inbound, msg) = (bot.name.clone(), ib.chat_id, ib.sender_id, ib.id, ib.msg_id);
    let assistant_ids = app
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            let mut prev = parent;
            for t in &turns {
                let id = conv::insert_turn_sync(
                    &tx,
                    &NewTurn {
                        parent_id: prev,
                        turn: t.clone(),
                        spec: None,
                        status: TurnStatus::Done,
                        bot: Some(bot_name.clone()),
                        chat_id: Some(chat),
                        user_id: user,
                        inbound_id: Some(inbound),
                    },
                )?;
                prev = Some(id);
            }
            // Only bot messages are mapped to turns: replies to the user's own message
            // are not continuations (same as the old bot).
            let last = prev.context("no user turn")?;
            let _ = msg;
            let mut ids = Vec::new();
            for s in &specs {
                ids.push(conv::insert_turn_sync(
                    &tx,
                    &NewTurn {
                        parent_id: Some(last),
                        turn: Turn { role: Role::Assistant, origin: Origin::Live, parts: vec![], native: None },
                        spec: Some(s.clone()),
                        status: TurnStatus::Pending,
                        bot: Some(bot_name.clone()),
                        chat_id: Some(chat),
                        user_id: user,
                        inbound_id: Some(inbound),
                    },
                )?);
            }
            tx.commit()?;
            Ok(ids)
        })
        .await?;
    info!(turns = ?assistant_ids, "created assistant turns");
    for id in assistant_ids {
        jobs::spawn_turn(app.clone(), id);
    }
    let _ = reply_target;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_references() {
        assert_eq!(
            youtube_refs(
                "see @https://www.youtube.com/watch?v=abc_12-3&t=5 and @https://youtu.be/xyz but not https://youtube.com/no"
            ),
            vec!["https://www.youtube.com/watch?v=abc_12-3&t=5".to_string(), "https://youtu.be/xyz".to_string()]
        );
    }

    #[test]
    fn history_turns_and_media_placement() {
        let img =
            Part::Image { blob: omni_core::BlobRef::for_bytes(b"x", "image/jpeg"), filename: None, generated: false };
        let t = user_turns("::system\nbe brief\n::user\nwhat is this?\n::assistant\nIt is", vec![img.clone()]).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].role, Role::System);
        assert_eq!(t[1].role, Role::User);
        assert!(matches!(t[1].parts[0], Part::Image { .. }));
        assert_eq!(t[2].role, Role::Assistant);
        assert_eq!(t[2].origin, Origin::Synthetic);
        assert!(user_turns("", vec![]).is_err());
        let t = user_turns("plain question", vec![]).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].origin, Origin::Live);
    }
}
