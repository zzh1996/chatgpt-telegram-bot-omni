//! Bot commands.

use std::sync::Arc;

use anyhow::{Context, Result};
use omni_core::content::{Part, ReasoningKind};
use omni_core::trigger;
use omni_markdown::rich::{CellAlign, RBlock, RCell, RItem, RText};
use rusqlite::params;
use serde_json::json;

use crate::access::{self, Access, Rule};
use crate::app::App;
use crate::compose::fmt_cost;
use crate::conv::{self, Inbound, TurnStatus};
use crate::jobs;
use crate::reply;
use crate::store::now_ms;
use crate::tg::bot::BotConn;
use crate::tg::outbox::{self, Link};

pub const HINT: &str = "Start a conversation with a prefix such as `$hello` (default model) or `alias$question`, or reply to one of my messages to continue. /help for details.";

const HELP: &str = r#"**Start a conversation**: `prefix$text`
- `$text` uses this bot's default model; `alias$text` uses an alias (see /models); `author/model$text` uses any OpenRouter model id directly (new models work without any config change). Everything after `$` is sent exactly as typed, including a leading space.
- Several models at once: `c,g,openai/gpt-6$ compare these`.
- Options after `+`: `c+s$` web search, `c+e=low$` reasoning effort, `+max=2000`, `+t=0.7`, `+p=any` (let OpenRouter pick the provider), `+pure` (no automatic max-capability presets), `+debug`, or a raw JSON patch `+{"temperature":0.2}`. Full list: /help options.
- Define a system prompt and fake history with marker lines:
```
c$::system
You are a pirate.
::user
hi
::assistant
Arr!
::user
who are you?
```
A line that literally is `::user` can be sent by escaping it as `\::user`.

**Continue**: reply to any of my messages. The whole reply is sent verbatim (a leading `\` is removed, so `\/raw` sends the text `/raw`). Replying to a failed message is rejected; use /retry on it.

**Commands (reply to a bot message)**: /m `spec$ text` continue with another model · /raw original Markdown and files · /info request details · /retry regenerate a failed answer · /continue keep writing an answer that stopped at the long-decode budget.

**Other**: /models [search] · /model `id` · /stats [today|7d|30d|all] · /usage · /whoami · /ping

By default every request maxes out capability (highest reasoning effort, max output tokens), pins the model's first-party provider on OpenRouter, and never adds a system prompt. In a reply, the first line names the model; everything else the bot adds (reasoning, sources, errors, notes, usage) is in collapsible blocks or small text, so the model's answer is always unambiguous."#;

const ADMIN_HELP: &str = r#"**Admin**: /allow [chat_id] · /deny [chat_id] · /allow_user `id` · /deny_user `id` · /ban `id` · /unban `id` · /acl · /quota `user|chat` `id|default` `capacity_usd` `refill_usd_per_day` (or `off`) · /alias `name` `spec` · /unalias `name` · /credits · /stats all"#;

fn label(s: &str) -> RText {
    RText::bold(RText::plain(s))
}

fn md(s: &str) -> Vec<RBlock> {
    omni_markdown::rich::markdown_to_blocks(s, 0, omni_markdown::rich::RichLimits::TELEGRAM)
}

fn table(header: &[&str], rows: Vec<Vec<String>>) -> RBlock {
    let mut out = vec![
        header
            .iter()
            .map(|h| RCell { text: RText::plain(*h), header: true, align: CellAlign::Left })
            .collect::<Vec<_>>(),
    ];
    for r in rows {
        out.push(
            r.into_iter()
                .enumerate()
                .map(|(i, c)| RCell {
                    text: RText::plain(c),
                    header: false,
                    align: if i == 0 { CellAlign::Left } else { CellAlign::Right },
                })
                .collect(),
        );
    }
    RBlock::Table { rows: out, bordered: true, striped: true }
}

async fn send(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, title: &str, mut blocks: Vec<RBlock>) {
    blocks.insert(0, RBlock::Paragraph(RText::concat(vec![RText::plain("🤖 "), label(title)])));
    reply::send_blocks(app, bot, ib.chat_id, ib.msg_id, blocks).await;
}

async fn say(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, text: &str) {
    reply::notice(app, bot, ib.chat_id, ib.msg_id, text).await;
}

pub async fn run(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, name: &str, args: &str) -> Result<()> {
    let user = ib.sender_id.unwrap_or(0);
    let admin = app.is_admin(user);
    let access = access::check(app, ib.chat_id, user).await?;
    let allowed = access == Access::Allowed;
    // Commands that work for everyone (useful to get ids for the admin).
    match name {
        "ping" => {
            let a = match &access {
                Access::Allowed => "allowed",
                Access::Denied(_) | Access::Silent => "not allowed",
            };
            let up = jiff::Timestamp::now().duration_since(app.started_at);
            say(
                app,
                bot,
                ib,
                &format!(
                    "pong · chat_id={} · user_id={} · {a} · bot={} · up {}s",
                    ib.chat_id,
                    user,
                    bot.name,
                    up.as_secs()
                ),
            )
            .await;
            return Ok(());
        }
        "whoami" => {
            say(
                app,
                bot,
                ib,
                &format!(
                    "user_id={user}{}{} · chat_id={}",
                    ib.payload.sender_username.as_ref().map(|u| format!(" (@{u})")).unwrap_or_default(),
                    if admin { " · admin" } else { "" },
                    ib.chat_id
                ),
            )
            .await;
            return Ok(());
        }
        _ => {}
    }
    if !allowed && !admin {
        if let Access::Denied(msg) = access {
            say(app, bot, ib, &format!("⛔ {msg}")).await;
        }
        return Ok(());
    }
    match name {
        "start" | "help" => {
            if args.trim() == "options" {
                send(
                    app,
                    bot,
                    ib,
                    "Options",
                    vec![RBlock::Pre {
                        text: omni_llm::openrouter::options::OPTIONS_HELP.into(),
                        language: String::new(),
                    }],
                )
                .await;
            } else {
                let mut b = md(HELP);
                if admin {
                    b.extend(md(ADMIN_HELP));
                }
                send(app, bot, ib, "Help", b).await;
            }
        }
        "models" => cmd_models(app, bot, ib, args.trim()).await?,
        "model" => cmd_model(app, bot, ib, args.trim()).await?,
        "raw" => cmd_raw(app, bot, ib).await?,
        "info" => cmd_info(app, bot, ib).await?,
        "retry" => cmd_retry(app, bot, ib).await?,
        "continue" => cmd_continue(app, bot, ib).await?,
        "m" => cmd_m(app, bot, ib, args).await?,
        "stats" => cmd_stats(app, bot, ib, args.trim(), admin).await?,
        "usage" => cmd_usage(app, bot, ib).await?,
        _ if !admin => say(app, bot, ib, "⛔ admin only").await,
        "allow" | "deny" => {
            let id = args.trim().parse::<i64>().unwrap_or(ib.chat_id);
            let status = if name == "allow" { Some("allow") } else { None };
            access::acl_set(&app.db, if id > 0 { "user" } else { "chat" }, id, status, user, None).await?;
            say(
                app,
                bot,
                ib,
                &format!("{} {id}", if name == "allow" { "✅ allowed" } else { "❎ removed from allow list:" }),
            )
            .await;
        }
        "allow_user" | "deny_user" | "ban" | "unban" => {
            let Ok(id) = args.trim().parse::<i64>() else {
                say(app, bot, ib, "usage: /allow_user <user_id>").await;
                return Ok(());
            };
            let status = match name {
                "allow_user" => Some("allow"),
                "ban" => Some("deny"),
                _ => None,
            };
            access::acl_set(&app.db, "user", id, status, user, None).await?;
            say(app, bot, ib, &format!("✅ user {id}: {}", status.unwrap_or("removed"))).await;
        }
        "acl" => {
            let rows = access::acl_list(&app.db).await?;
            let rows = rows
                .into_iter()
                .map(|(scope, id, status, name)| vec![scope, id.to_string(), status, name.unwrap_or_default()])
                .collect();
            send(app, bot, ib, "Access list", vec![table(&["scope", "id", "status", "name"], rows)]).await;
        }
        "quota" => cmd_quota(app, bot, ib, args, user).await?,
        "alias" => {
            let mut it = args.trim().splitn(2, char::is_whitespace);
            let (Some(n), Some(spec)) = (it.next(), it.next()) else {
                say(app, bot, ib, "usage: /alias <name> <spec>, e.g. /alias g5 openrouter:google/gemini-5+e=high")
                    .await;
                return Ok(());
            };
            if trigger::parse_spec(spec.trim()).is_err() || n.contains(['$', ',', '+', '{']) {
                say(app, bot, ib, "⚠️ invalid alias name or spec").await;
                return Ok(());
            }
            let (n2, s2) = (n.to_string(), spec.trim().to_string());
            app.db
                .call(move |c| {
                    c.execute(
                        "INSERT INTO aliases(name, spec, created_by, created_at) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(name) DO UPDATE SET spec = excluded.spec, created_by = excluded.created_by, created_at = excluded.created_at",
                        params![n2, s2, user, now_ms()],
                    )?;
                    Ok(())
                })
                .await?;
            app.aliases.write().insert(n.to_string(), spec.trim().to_string());
            say(app, bot, ib, &format!("✅ alias {n} = {}", spec.trim())).await;
        }
        "unalias" => {
            let n = args.trim().to_string();
            let n2 = n.clone();
            app.db.call(move |c| Ok(c.execute("DELETE FROM aliases WHERE name = ?1", params![n2])?)).await?;
            app.aliases.write().remove(&n);
            if let Some(a) = app.cfg.aliases.iter().find(|a| a.name == n) {
                app.aliases.write().insert(n.clone(), a.spec.clone());
                say(app, bot, ib, &format!("runtime alias removed; config alias {n} = {} remains", a.spec)).await;
            } else {
                say(app, bot, ib, &format!("✅ alias {n} removed")).await;
            }
        }
        "credits" => match app.openrouter.key_info().await {
            Ok(v) => {
                send(
                    app,
                    bot,
                    ib,
                    "OpenRouter key",
                    vec![RBlock::Pre { text: serde_json::to_string_pretty(&v)?, language: "json".into() }],
                )
                .await
            }
            Err(e) => say(app, bot, ib, &format!("⚠️ {e:#}")).await,
        },
        _ => {}
    }
    Ok(())
}

async fn cmd_models(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, query: &str) -> Result<()> {
    if !query.is_empty() {
        let found = app.openrouter.catalog.search(query, 40);
        let rows = found
            .iter()
            .map(|m| {
                let effort = m.max_effort().unwrap_or("-").to_string();
                vec![m.id.clone(), m.architecture.input_modalities.join(","), effort]
            })
            .collect();
        send(
            app,
            bot,
            ib,
            &format!("OpenRouter models matching `{query}`"),
            vec![table(&["model id", "input", "max effort"], rows)],
        )
        .await;
        return Ok(());
    }
    let cfg = app.bot_cfg(&bot.name);
    let aliases = app.aliases.read().clone();
    let rows: Vec<Vec<String>> = aliases
        .iter()
        .filter(|(_, spec)| {
            let canonical = trigger::parse_spec(spec)
                .map(|s| if s.model.contains(':') { s.model } else { format!("openrouter:{}", s.model) })
                .unwrap_or_default();
            app.responsible_bot(ib.chat_id, &canonical, &bot.name).as_deref() == Some(bot.name.as_str())
        })
        .map(|(n, s)| vec![format!("{n}$"), s.clone()])
        .collect();
    let mut blocks = vec![table(&["prefix", "spec"], rows)];
    if let Some(d) = cfg.and_then(|c| c.default_model.clone()) {
        blocks.push(RBlock::Paragraph(RText::plain(format!("Bare `$` uses: {d}"))));
    }
    blocks.push(RBlock::Paragraph(RText::plain(
        "Any OpenRouter model id works as a prefix too, e.g. openai/gpt-6$ — search with /models <text>.",
    )));
    send(app, bot, ib, "Models", blocks).await;
    Ok(())
}

async fn cmd_model(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, arg: &str) -> Result<()> {
    let Some(cfg) = app.bot_cfg(&bot.name) else { return Ok(()) };
    let token = trigger::parse_spec(if arg.is_empty() { "" } else { arg }).ok();
    let spec = match token {
        Some(t) => app.resolve_spec(&t, cfg, false).await.ok().flatten(),
        None => None,
    };
    let Some(spec) = spec else {
        say(app, bot, ib, "usage: /model <alias or model id>").await;
        return Ok(());
    };
    let info = app.openrouter.model_info(&spec.model).await;
    let mut blocks = vec![];
    match info {
        Some(m) => {
            blocks.push(RBlock::Pre { text: serde_json::to_string_pretty(&*m)?, language: "json".into() });
            match app.openrouter.endpoints(&spec.model).await {
                Ok(eps) => {
                    let first = app.openrouter.config().fidelity.first_party_for(&spec.model).map(str::to_string);
                    let rows = eps
                        .iter()
                        .map(|e| {
                            let tag = e.tag.clone().unwrap_or_default();
                            let mark = if first.as_deref() == e.base_slug() { " (first-party)" } else { "" };
                            vec![
                                format!("{tag}{mark}"),
                                e.quantization.clone().unwrap_or_default(),
                                e.max_completion_tokens.map(|x| x.to_string()).unwrap_or_default(),
                            ]
                        })
                        .collect();
                    blocks.push(table(&["endpoint", "quant", "max out"], rows));
                }
                Err(e) => blocks.push(RBlock::Paragraph(RText::plain(format!("endpoints: {e:#}")))),
            }
        }
        None => {
            blocks.push(RBlock::Paragraph(RText::plain(format!("{} is not in the OpenRouter catalog", spec.model))))
        }
    }
    send(app, bot, ib, &format!("{} → {}{}", spec.label, spec.model, spec.options_syntax()), blocks).await;
    Ok(())
}

/// The assistant turn a reply command targets.
async fn target_turn(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<Option<conv::TurnRow>> {
    let Some(rt) = ib.reply_to_msg_id else {
        say(app, bot, ib, "reply to one of my messages with this command").await;
        return Ok(None);
    };
    let Some((turn_id, _)) = conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await? else {
        say(app, bot, ib, "that message is not one of my answers").await;
        return Ok(None);
    };
    Ok(Some(conv::get_turn(&app.db, turn_id).await?))
}

/// Raw answers up to this size are sent inline (code block with a copy button), longer ones as a file.
const RAW_INLINE_MAX: usize = 2000;

async fn cmd_raw(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    let mut md = String::new();
    let mut reasoning = String::new();
    let mut answer = String::new();
    for p in &t.turn.parts {
        match p {
            Part::Reasoning(r) => {
                if let Some(text) = &r.text {
                    let kind = if r.kind == ReasoningKind::Summary { "reasoning summary" } else { "reasoning" };
                    md.push_str(&format!("<!-- {kind} -->\n{text}\n<!-- end {kind} -->\n\n"));
                    if !reasoning.is_empty() {
                        reasoning.push_str("\n\n");
                    }
                    reasoning.push_str(text);
                }
            }
            Part::Text { text, .. } => {
                md.push_str(text);
                answer.push_str(text);
            }
            Part::Refusal { text } => {
                md.push_str(&format!("<!-- refusal -->\n{text}\n"));
                answer.push_str(text);
            }
            _ => {}
        }
    }
    if answer.chars().count() <= RAW_INLINE_MAX && reasoning.chars().count() <= 28_000 {
        let mut blocks = Vec::new();
        if !reasoning.is_empty() {
            blocks.push(RBlock::Details {
                open: false,
                title: RText::plain("💭 Reasoning (raw)"),
                blocks: vec![RBlock::Pre { text: reasoning, language: "markdown".into() }],
            });
        }
        blocks.push(RBlock::Pre { text: answer, language: "markdown".into() });
        reply::send_blocks(app, bot, ib.chat_id, ib.msg_id, blocks).await;
    } else {
        let name = format!("answer-{}.md", t.id);
        let link = Link { turn_id: -(ib.msg_id as i64), attempt_id: None, part_index: 9000 };
        if let Err(e) = outbox::send_file(
            &app.db,
            bot,
            ib.chat_id,
            Some(ib.msg_id),
            md.into_bytes(),
            &name,
            "text/markdown",
            false,
            "",
            link,
        )
        .await
        {
            say(app, bot, ib, &format!("⚠️ {e}")).await;
        }
    }
    let mut i = 0;
    for p in &t.turn.parts {
        if let Part::Image { blob, .. } | Part::File { blob, .. } = p {
            i += 1;
            let bytes = app.blobs.get(blob).await?;
            let ext = blob.mime.rsplit('/').next().unwrap_or("bin");
            let link = Link { turn_id: -(ib.msg_id as i64), attempt_id: None, part_index: 9000 + i };
            let _ = outbox::send_file(
                &app.db,
                bot,
                ib.chat_id,
                Some(ib.msg_id),
                bytes,
                &format!("file-{}-{i}.{ext}", t.id),
                &blob.mime,
                false,
                "",
                link,
            )
            .await;
        }
    }
    Ok(())
}

async fn cmd_info(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    let attempts = conv::attempts_of(&app.db, t.id).await?;
    let mut blocks = Vec::new();
    if let Some(s) = &t.spec {
        blocks.push(RBlock::Paragraph(RText::plain(format!(
            "model: {} · options: {} · status: {:?}",
            s.canonical(),
            s.options_syntax(),
            t.status
        ))));
    }
    let mut dump = json!({"turn": t.id, "attempts": []});
    for a in &attempts {
        let (req, _out, err, up, usage) = conv::attempt_json(&app.db, a.id).await?;
        dump["attempts"].as_array_mut().unwrap().push(json!({
            "n": a.n, "status": a.status, "upstream_id": up, "usage": usage, "error": err, "request": req,
        }));
        blocks.push(RBlock::Paragraph(RText::plain(format!(
            "attempt {} · {} · {}",
            a.n,
            a.status,
            up.unwrap_or_default()
        ))));
    }
    send(app, bot, ib, "Request details", blocks).await;
    let link = Link { turn_id: -(ib.msg_id as i64), attempt_id: None, part_index: 9500 };
    let body = serde_json::to_vec_pretty(&dump)?;
    if let Err(e) = outbox::send_file(
        &app.db,
        bot,
        ib.chat_id,
        Some(ib.msg_id),
        body,
        &format!("request-{}.json", t.id),
        "application/json",
        false,
        "",
        link,
    )
    .await
    {
        say(app, bot, ib, &format!("⚠️ {e}")).await;
    }
    Ok(())
}

async fn cmd_retry(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if t.status != TurnStatus::Failed {
        say(app, bot, ib, "only failed answers can be retried; to regenerate, reply to the previous message again")
            .await;
        return Ok(());
    }
    // Reset the failure count by marking old failed attempts as "superseded".
    let id = t.id;
    app.db
        .call(move |c| {
            c.execute(
                "UPDATE attempts SET status = 'superseded' WHERE turn_id = ?1 AND status = 'failed'",
                params![id],
            )?;
            c.execute("UPDATE turns SET status = 'pending', updated_at = ?1 WHERE id = ?2", params![now_ms(), id])?;
            Ok(())
        })
        .await?;
    jobs::spawn_turn(app.clone(), id);
    Ok(())
}

async fn cmd_continue(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if t.status != TurnStatus::Done || !jobs::can_continue(app, t.id).await? {
        say(
            app,
            bot,
            ib,
            "only answers that stopped at the long-decode output budget can be continued (a model with long decode, \
             footer shows finish: length (MAX_TOKENS))",
        )
        .await;
        return Ok(());
    }
    if !jobs::spawn_continue(app.clone(), t.id, ib.msg_id) {
        say(app, bot, ib, "this answer is still being generated").await;
    }
    Ok(())
}

async fn cmd_m(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, args: &str) -> Result<()> {
    if ib.reply_to_msg_id.is_none() {
        say(app, bot, ib, "reply to one of my messages: /m spec$ text").await;
        return Ok(());
    }
    let t = match trigger::parse_trigger(args) {
        Ok(Some(t)) => t,
        Ok(None) => {
            say(app, bot, ib, "usage: /m spec$ text (e.g. /m g+s$ what do you think?)").await;
            return Ok(());
        }
        Err(e) => {
            say(app, bot, ib, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    let specs = match crate::handler::resolve_specs(app, bot, ib.chat_id, &t.specs).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => {
            say(app, bot, ib, "⚠️ none of these models is served by this bot here").await;
            return Ok(());
        }
        Err(e) => {
            say(app, bot, ib, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    crate::handler::continue_conversation(app, bot, ib, Some((specs, t.text))).await
}

fn period_start(app: &App, p: &str) -> (i64, &'static str) {
    let tz = jiff::tz::TimeZone::get(&app.cfg.general.timezone).unwrap_or(jiff::tz::TimeZone::UTC);
    let now = jiff::Timestamp::now().to_zoned(tz);
    let day_start = now.start_of_day().map(|z| z.timestamp().as_millisecond()).unwrap_or(0);
    match p {
        "today" => (day_start, "today"),
        "7d" => (day_start - 6 * 86_400_000, "last 7 days"),
        "all" => (0, "all time"),
        _ => (day_start - 29 * 86_400_000, "last 30 days"),
    }
}

async fn cmd_stats(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, args: &str, admin: bool) -> Result<()> {
    let mut words = args.split_whitespace().collect::<Vec<_>>();
    let global = admin && words.first() == Some(&"all") && ib.chat_id > 0;
    if global {
        words.remove(0);
    }
    let (since, period) = period_start(app, words.first().copied().unwrap_or("30d"));
    let chat = ib.chat_id;
    let user = ib.sender_id.unwrap_or(0);
    // Scope: in a group -> this group; in private -> the user's own usage everywhere; admin `all` -> everything.
    let (where_sql, scope_desc): (&str, String) = if global {
        ("1 = 1", "all chats".into())
    } else if chat < 0 {
        ("chat_id = ?2", "this chat".into())
    } else {
        ("user_id = ?3", "your usage in all chats".into())
    };
    let (by_model, by_who) = app
        .db
        .call(move |c| {
            let q = |group: &str| {
                format!(
                    "SELECT {group}, COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(SUM(cost_usd),0)
                     FROM usage_ledger WHERE ts >= ?1 AND {where_sql} AND (?2 = ?2) AND (?3 = ?3) GROUP BY 1 ORDER BY 5 DESC LIMIT 30"
                )
            };
            let run = |sql: String| -> rusqlite::Result<Vec<(String, i64, i64, i64, f64)>> {
                let mut s = c.prepare(&sql)?;
                s.query_map(params![since, chat, user], |r| {
                    Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect()
            };
            let by_model = run(q("model"))?;
            let who = if global {
                "COALESCE((SELECT title FROM chats WHERE chats.chat_id = usage_ledger.chat_id), CAST(chat_id AS TEXT))"
            } else if chat < 0 {
                "COALESCE((SELECT TRIM(COALESCE(first_name,'') || ' ' || COALESCE(last_name,'')) FROM users WHERE users.user_id = usage_ledger.user_id), CAST(user_id AS TEXT))"
            } else {
                "COALESCE((SELECT title FROM chats WHERE chats.chat_id = usage_ledger.chat_id), CAST(chat_id AS TEXT))"
            };
            let by_who = run(q(who))?;
            Ok((by_model, by_who))
        })
        .await?;
    let fmt = |rows: Vec<(String, i64, i64, i64, f64)>| -> Vec<Vec<String>> {
        rows.into_iter()
            .map(|(k, n, i, o, c)| vec![k, n.to_string(), i.to_string(), o.to_string(), fmt_cost(c)])
            .collect()
    };
    let total: f64 = by_model.iter().map(|r| r.4).sum();
    let who_header = if chat < 0 && !global { "user" } else { "chat" };
    let blocks = vec![
        RBlock::Paragraph(RText::plain(format!("{scope_desc} · {period} · total {}", fmt_cost(total)))),
        table(&["model", "calls", "in", "out", "cost"], fmt(by_model)),
        table(&[who_header, "calls", "in", "out", "cost"], fmt(by_who)),
    ];
    send(app, bot, ib, "Usage stats", blocks).await;
    Ok(())
}

async fn cmd_usage(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let user = ib.sender_id.unwrap_or(0);
    let mut lines = Vec::new();
    for (scope, id) in [("user", user), ("chat", ib.chat_id)] {
        if scope == "chat" && ib.chat_id > 0 {
            continue;
        }
        match access::level(&app.db, scope, id).await? {
            Some((lvl, r)) => lines.push(format!(
                "{scope} {id}: {lvl:.4} / {:.2} USD, refills {:.2} USD/day",
                r.capacity, r.refill_per_day
            )),
            None => lines.push(format!("{scope} {id}: no limit")),
        }
    }
    let items =
        lines.into_iter().map(|l| RItem { checkbox: None, blocks: vec![RBlock::Paragraph(RText::plain(l))] }).collect();
    send(app, bot, ib, "Spend limits", vec![RBlock::List { items }]).await;
    Ok(())
}

async fn cmd_quota(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, args: &str, by: i64) -> Result<()> {
    let w: Vec<&str> = args.split_whitespace().collect();
    let usage = "usage: /quota user|chat <id|default> <capacity_usd> <refill_usd_per_day>  — or: /quota user|chat <id|default> off";
    let scope = match w.first() {
        Some(&"user") => "user",
        Some(&"chat") => "chat",
        _ => {
            say(app, bot, ib, usage).await;
            return Ok(());
        }
    };
    let id = match w.get(1) {
        Some(&"default") => 0,
        Some(s) => match s.parse::<i64>() {
            Ok(i) => i,
            Err(_) => {
                say(app, bot, ib, usage).await;
                return Ok(());
            }
        },
        None => {
            say(app, bot, ib, usage).await;
            return Ok(());
        }
    };
    let rule = match (w.get(2), w.get(3)) {
        (Some(&"off"), _) => None,
        (Some(c), Some(r)) => match (c.parse::<f64>(), r.parse::<f64>()) {
            (Ok(c), Ok(r)) => Some(Rule { capacity: c, refill_per_day: r }),
            _ => {
                say(app, bot, ib, usage).await;
                return Ok(());
            }
        },
        _ => {
            say(app, bot, ib, usage).await;
            return Ok(());
        }
    };
    access::set_rule(&app.db, scope, id, rule, by).await.context("set quota")?;
    say(
        app,
        bot,
        ib,
        &format!(
            "✅ {scope} {} quota: {}",
            if id == 0 { "default".to_string() } else { id.to_string() },
            match rule {
                Some(r) => format!("{} USD capacity, {} USD/day", r.capacity, r.refill_per_day),
                None => "off".into(),
            }
        ),
    )
    .await;
    Ok(())
}
