//! Bot commands.

use std::sync::Arc;

use anyhow::{Context, Result};
use omni_core::content::Part;
use omni_core::trigger;
use omni_markdown::rich::{CellAlign, RBlock, RCell, RItem, RText};
use rusqlite::params;
use serde_json::json;

use crate::access::{self, Access};
use crate::app::App;
use crate::compose::fmt_cost;
use crate::conv::{self, Inbound, TurnStatus};
use crate::jobs;
use crate::reply;
use crate::store::now_ms;
use crate::tg::bot::BotConn;
use crate::tg::upload::{Source, Want};
use grammers_tl_types as tl;

/// The hint for messages that are neither a prefix nor a reply, with `marker`.
pub fn hint(marker: &str) -> String {
    with_marker(HINT, marker)
}

/// `$` in these texts always stands for the configured trigger marker (and is
/// always inside a code span, so any marker keeps the Markdown intact).
fn with_marker(text: &str, marker: &str) -> String {
    text.replace('$', marker)
}

/// How model ids can be typed directly as prefixes, from the providers that
/// allow it (`allow_unlisted_models`); `None` if none does.
pub fn direct_ids(app: &App) -> Option<String> {
    let m = app.marker();
    let mut forms = Vec::new();
    if app.unlisted_models_allowed("openrouter") {
        forms.push(format!("an OpenRouter model id (`author/model{m}`)"));
    }
    if app.unlisted_models_allowed("ark") {
        forms.push(format!("a Volcengine Ark model id (`ark:model{m}`)"));
    }
    (!forms.is_empty()).then(|| forms.join(" or "))
}

/// Commands that start requests that cost money. Every other non-admin command
/// only shows information: answered for everyone in an allowed chat, people
/// banned there included. Where the chat is not allowed only /ping and an
/// administrator's /allow are answered.
const PAID_COMMANDS: &[&str] = &["m", "retry", "continue"];

/// The answer to a paid request from someone banned in the chat.
pub const BANNED: &str = "⛔ You are banned from using this bot in this chat.";

const HINT: &str = "Start a conversation with `prefix$question` (the prefixes are listed by /models), or reply to one of my messages to continue. /help for details.";

const HELP: &str = r#"**Start a conversation**: `prefix$text`
- PREFIXES Everything after `$` is sent exactly as typed, including a leading space.
- Several models at once: `c,g$compare these`. (Examples use prefixes of the default configuration; /models lists this bot's.)
- Options after `+`: `c+s$` the provider's own web search, `c+e=low$` reasoning effort (`e=none` turns reasoning off), `gi+4W$` image size and aspect ratio, `+p=<provider>` another official provider of the model. The full (closed) list: /help options; anything else is an error.
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
Short forms: `::sys`, `::u`, `::a` (or `::model`). A line that literally is `::user` is sent by writing `\::user` (one backslash is removed).

**Continue**: reply to any of my messages. The whole reply is the next message, exactly as typed (marker lines are plain text there), except that a leading `\` is removed, so `\/raw` sends the text `/raw`. Replying to a failed message is rejected; use /retry on it.

**Prefixes of this chat**: anyone allowed here (and not banned) can make a custom chatbot for this chat: a model with its options, a system prompt and a history put before every new message. `/prefix name prefix$::system …` (with `::user` / `::assistant` lines for the history) creates `name$`; or reply `/prefix name` to one of my answers to turn that conversation into one. Names: 3–16 letters and digits, not taken. Anyone here can use it; replies, billing and stats are the model's own. /prefix lists this chat's, /prefix `name` shows one, /unprefix `name` deletes one (its creator or an admin; replies to its answers keep working).

**Commands (reply to a bot message)**: /m `prefix$text` continue with another model · /raw original Markdown and files · /dump the whole conversation up to that message as a JSON file · /info request details · /retry regenerate a failed answer · /continue keep writing an answer that stopped at the long-decode budget.

**Other**: /models [search] · /model `prefix` · /stats [today|7d|30d|all] this chat's usage (calendar days in the bot's time zone, today since 00:00, default 30d; in a group also one member: `/stats user_id`, or reply /stats to their message) · /usage · /ping (ids and access here)

Where the bot is not allowed, only /ping (and an admin's /allow) is answered. Where it is, commands that only show information work for everyone, also for people banned there; requests that cost money tell banned people that they are banned.

By default every request maxes out capability (highest reasoning effort, max output tokens), pins the model's first-party provider on OpenRouter, and never adds a system prompt. In a reply, the first line names the model; everything else the bot adds (reasoning, sources, errors, notes, usage) is in collapsible blocks or small text, so the model's answer is always unambiguous."#;

/// Commands only administrators may run: in their private chat for anything of
/// this bot, inside a group only those in [`GROUP_ADMIN_COMMANDS`], for that group.
const ADMIN_COMMANDS: &[&str] = &["allow", "deny", "ban", "unban", "acl", "quota", "alias", "unalias", "credits"];
/// Admin commands that work inside a group; they only see and change that group,
/// never anything about other chats.
const GROUP_ADMIN_COMMANDS: &[&str] = &["allow", "deny", "ban", "unban", "acl", "quota"];

const ADMIN_HELP_PRIVATE: &str = r#"**Admin** (in private chat: every chat of this bot; group ids are negative, a private chat's id is the user's id): /allow `chat_id` · /deny `chat_id` · /ban `user_id` `group_id` · /unban `user_id` `group_id` · /acl · /quota [`chat_id` [`chat`|`members`|`user_id` [`capacity_usd` `refill_usd_per_day` | `off`]]] · /stats [period] (every chat: rankings by model, chat, user and chat · user; one of them, longer: /stats `model|chat|user|chat-user` [period]) · /stats `chat_id` [`user_id`] [period] · /prefix `chat_id` (a chat's own prefixes) · /unprefix `chat_id` `name` · /alias `prefix$` `spec` · /unalias `prefix$` · /credits"#;

const ADMIN_HELP_GROUP: &str = r#"**Admin** (in a group: this group only): /allow (also where the group is not allowed yet) · /deny · /ban `user_id` (or reply to their message) · /unban `user_id` (or reply) · /acl · /quota [`chat`|`members`|`user_id` [`capacity_usd` `refill_usd_per_day` | `off`]] (`chat`: the whole group; `members`: each member by default; as a reply: that person) · /unprefix `name` (anyone's). Everything else in private chat."#;

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
    let access = access::check(app, &bot.name, ib.chat_id, user).await?;
    if name == "ping" {
        // Everything an administrator needs to allow a chat or ban someone.
        let here = match access {
            Access::Allowed => "this chat is allowed",
            Access::Banned => "this chat is allowed, you are banned here",
            Access::NotAllowed => "this chat is not allowed",
        };
        let up = jiff::Timestamp::now().duration_since(app.started_at);
        say(
            app,
            bot,
            ib,
            &format!(
                "pong · chat_id={} · user_id={}{}{} · {here} · bot={} · up {}s",
                ib.chat_id,
                user,
                ib.payload.sender_username.as_ref().map(|u| format!(" (@{u})")).unwrap_or_default(),
                if admin { " · admin" } else { "" },
                bot.name,
                up.as_secs()
            ),
        )
        .await;
        return Ok(());
    }
    // Where the chat is not allowed, only /ping is answered, and an
    // administrator's /allow (owner's rule): in a group it allows that group,
    // in their private chat any chat, their own included.
    if access == Access::NotAllowed && !(admin && name == "allow") {
        return Ok(());
    }
    if ADMIN_COMMANDS.contains(&name) {
        // In a group scoped to that group; everything else from the private chat.
        if !admin {
            say(app, bot, ib, "⛔ admin only").await;
        } else if ib.chat_id < 0 && !GROUP_ADMIN_COMMANDS.contains(&name) {
            say(
                app,
                bot,
                ib,
                &format!("⛔ In a group, admin commands only act on that group. Use /{name} in private chat."),
            )
            .await;
        } else {
            admin_command(app, bot, ib, name, args, user).await?;
        }
        return Ok(());
    }
    // Paid requests: not when banned here (queries are answered even then).
    if PAID_COMMANDS.contains(&name) && access == Access::Banned {
        say(app, bot, ib, BANNED).await;
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
                    vec![RBlock::Pre { text: with_marker(omni_llm::request::HELP, app.marker()), language: String::new() }],
                )
                .await;
            } else {
                let prefixes = match direct_ids(app) {
                    Some(ids) => format!(
                        "The prefix is one of those listed by /models, or a model id typed directly: {ids}, for any model \
                         this bot has, so new models need no config change."
                    ),
                    None => "The prefix is one of those listed by /models.".to_string(),
                };
                let mut b = md(&with_marker(HELP, app.marker()).replacen("PREFIXES", &prefixes, 1));
                if admin {
                    let admin_help = if ib.chat_id < 0 { ADMIN_HELP_GROUP } else { ADMIN_HELP_PRIVATE };
                    b.extend(md(&with_marker(admin_help, app.marker())));
                }
                send(app, bot, ib, "Help", b).await;
            }
        }
        "models" => cmd_models(app, bot, ib, args.trim()).await?,
        "model" => cmd_model(app, bot, ib, args.trim()).await?,
        "raw" => cmd_raw(app, bot, ib).await?,
        "info" => cmd_info(app, bot, ib).await?,
        "dump" => cmd_dump(app, bot, ib).await?,
        "prefix" => cmd_prefix(app, bot, ib, args, access, admin).await?,
        "unprefix" => cmd_unprefix(app, bot, ib, args, admin).await?,
        "retry" => cmd_retry(app, bot, ib).await?,
        "continue" => cmd_continue(app, bot, ib).await?,
        "m" => cmd_m(app, bot, ib, args).await?,
        "stats" => cmd_stats(app, bot, ib, args.trim(), admin).await?,
        "usage" => cmd_usage(app, bot, ib).await?,
        _ => {}
    }
    Ok(())
}

/// `/credits`: the OpenRouter key's limit and spending, readable, with the full
/// response folded underneath (without the key's masked label).
fn credits_blocks(v: &serde_json::Value) -> Result<Vec<RBlock>> {
    let d = v.get("data").unwrap_or(v);
    let num = |k: &str| d.get(k).and_then(serde_json::Value::as_f64);
    let usd = |x: Option<f64>| x.map(fmt_cost).unwrap_or_else(|| "-".into());
    let mut lines = Vec::new();
    match num("limit") {
        Some(limit) => {
            let reset = d.get("limit_reset").and_then(|x| x.as_str()).map(|r| format!(" · resets {r}")).unwrap_or_default();
            lines.push(format!(
                "limit {} · used {} · remaining {}{reset}",
                usd(Some(limit)),
                usd(num("usage")),
                usd(num("limit_remaining"))
            ));
        }
        None => lines.push(format!("no limit · used {}", usd(num("usage")))),
    }
    lines.push(format!(
        "spent today {} · this week {} · this month {} (UTC)",
        usd(num("usage_daily")),
        usd(num("usage_weekly")),
        usd(num("usage_monthly"))
    ));
    if num("byok_usage").is_some_and(|b| b > 0.0) {
        let counted = d.get("include_byok_in_limit").and_then(|x| x.as_bool()) == Some(true);
        lines.push(format!(
            "with your own provider keys (BYOK): today {} · this week {} · this month {} ({} the limit)",
            usd(num("byok_usage_daily")),
            usd(num("byok_usage_weekly")),
            usd(num("byok_usage_monthly")),
            if counted { "counted in" } else { "not counted in" }
        ));
    }
    if let Some(f) = d.get("free_model_daily_requests")
        && let (Some(used), Some(limit)) = (f.get("used").and_then(|x| x.as_i64()), f.get("limit").and_then(|x| x.as_i64()))
    {
        lines.push(format!("free models: {used} of {limit} requests today"));
    }
    lines.push(match d.get("expires_at").and_then(|x| x.as_str()) {
        Some(e) => format!("the key expires {e}"),
        None => "the key does not expire".into(),
    });
    if d.get("is_free_tier").and_then(|x| x.as_bool()) == Some(true) {
        lines.push("free tier account".into());
    }
    let items =
        lines.into_iter().map(|l| RItem { checkbox: None, blocks: vec![RBlock::Paragraph(RText::plain(l))] }).collect();
    let mut raw = d.clone();
    if let Some(o) = raw.as_object_mut() {
        o.remove("label");
    }
    Ok(vec![
        RBlock::List { items },
        RBlock::Details {
            open: false,
            title: RText::plain("full response"),
            blocks: vec![RBlock::Pre { text: serde_json::to_string_pretty(&raw)?, language: "json".into() }],
        },
    ])
}

/// How a chat id reads: a group, or a user's private chat.
fn place_label(chat_id: i64) -> String {
    if chat_id > 0 { format!("private chat {chat_id}") } else { format!("group {chat_id}") }
}

/// The sender of the message `ib` replies to (for `/ban` as a reply).
async fn replied_sender(app: &App, bot: &BotConn, ib: &Inbound) -> Option<i64> {
    let rt = ib.reply_to_msg_id?;
    let msg = bot.get_messages(ib.chat_id, &[rt]).await.ok()?.into_iter().next()?;
    let tl::enums::Message::Message(m) = msg else { return None };
    let tl::enums::Peer::User(u) = m.from_id? else { return None };
    (!app.our_bot_ids().contains(&u.user_id)).then_some(u.user_id)
}

/// A prefix as typed in `/alias`, `/unalias`, `/model`: `name$` or `name`
/// (the empty prefix is `$`).
fn prefix_name<'a>(arg: &'a str, marker: &str) -> &'a str {
    arg.strip_suffix(marker).unwrap_or(arg)
}

/// Admin commands. In a group they only see and change that group.
async fn admin_command(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, name: &str, args: &str, by: i64) -> Result<()> {
    let group = ib.chat_id < 0;
    let here = ib.chat_id;
    let words: Vec<&str> = args.split_whitespace().collect();
    let marker = app.marker();
    match name {
        "allow" | "deny" => {
            let target = match words.first() {
                None => here,
                Some(w) => match w.parse::<i64>() {
                    Ok(id) if id != 0 => id,
                    _ => {
                        say(app, bot, ib, &format!("usage: /{name} [chat_id]")).await;
                        return Ok(());
                    }
                },
            };
            if group && target != here {
                say(app, bot, ib, &format!("⛔ In a group, /{name} only acts on this group. Use private chat for others."))
                    .await;
                return Ok(());
            }
            let allow = name == "allow";
            let changed = access::set_place(&app.db, &bot.name, target, allow, by).await?;
            let what = if target == here && group { "this group".to_string() } else { place_label(target) };
            let text = match (allow, changed) {
                (true, true) => format!("✅ allowed: {what}"),
                (true, false) => format!("already allowed: {what}"),
                (false, true) => format!("❎ removed from the allow list: {what}"),
                (false, false) => format!("not on the allow list: {what}"),
            };
            say(app, bot, ib, &text).await;
        }
        "ban" | "unban" => {
            // A ban lives in one group: this one, or the one named in private chat.
            let usage = if group {
                format!("usage: /{name} <user_id>, or reply /{name} to their message")
            } else {
                format!("usage: /{name} <user_id> <group_id>")
            };
            let user = match words.first() {
                Some(w) => w.parse::<i64>().ok(),
                None => replied_sender(app, bot, ib).await,
            };
            let Some(user) = user.filter(|u| *u > 0) else {
                say(app, bot, ib, &usage).await;
                return Ok(());
            };
            let place = if group {
                here
            } else {
                match words.get(1).map(|w| w.parse::<i64>()) {
                    Some(Ok(g)) if g < 0 => g,
                    _ => {
                        say(app, bot, ib, &usage).await;
                        return Ok(());
                    }
                }
            };
            let ban = name == "ban";
            let changed = access::set_ban(&app.db, &bot.name, place, user, ban, by).await?;
            let scope = if group { "in this group".to_string() } else { format!("in group {place}") };
            let text = match (ban, changed) {
                (true, true) => format!("🚫 user {user} banned {scope}"),
                (true, false) => format!("user {user} was already banned {scope}"),
                (false, true) => format!("✅ user {user} no longer banned {scope}"),
                (false, false) => format!("user {user} was not banned {scope}"),
            };
            say(app, bot, ib, &text).await;
        }
        "acl" => {
            let rows = access::rules(&app.db, &bot.name, group.then_some(here)).await?;
            let rows: Vec<Vec<String>> = rows
                .into_iter()
                .map(|r| {
                    let rule = if r.status == "allow" { "allowed" } else { "banned" };
                    let place = if group { "this group".to_string() } else { place_label(r.chat_id) };
                    let who = if r.user_id == 0 { "everyone".to_string() } else { r.user_id.to_string() };
                    let names = [r.place.filter(|_| !group), r.user].into_iter().flatten().collect::<Vec<_>>().join(" · ");
                    vec![rule.to_string(), place, who, names]
                })
                .collect();
            let title = if group { "Access rules of this group" } else { "Access rules" };
            let mut blocks = vec![table(&["rule", "where", "who", "names"], rows.clone())];
            if rows.is_empty() {
                blocks = vec![RBlock::Paragraph(RText::plain(if group {
                    "This group is not allowed and nobody is banned here."
                } else {
                    "No chat is allowed: only administrators can use this bot (in their private chats)."
                }))];
            }
            send(app, bot, ib, title, blocks).await;
        }
        "quota" => cmd_quota(app, bot, ib, &words, by).await?,
        "alias" => {
            let mut it = args.trim().splitn(2, char::is_whitespace);
            let (Some(n), Some(spec)) = (it.next(), it.next()) else {
                say(
                    app,
                    bot,
                    ib,
                    &with_marker(
                        "usage: /alias <prefix$> <spec>, e.g. /alias g5$ openrouter:google/gemini-5+e=high (the empty prefix is `$`)",
                        marker,
                    ),
                )
                .await;
                return Ok(());
            };
            let n = prefix_name(n, marker);
            let spec = spec.trim();
            if let Err(e) = trigger::parse_spec(spec) {
                say(app, bot, ib, &format!("⚠️ invalid spec: {e}")).await;
                return Ok(());
            }
            if !trigger::alias_name_ok(n, marker) {
                say(app, bot, ib, "⚠️ invalid prefix: no whitespace, `,+{}` or the marker inside").await;
                return Ok(());
            }
            let (b2, n2, s2) = (bot.name.clone(), n.to_string(), spec.to_string());
            app.db
                .call(move |c| {
                    c.execute(
                        "INSERT INTO aliases(bot, name, spec, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(bot, name) DO UPDATE SET spec = excluded.spec, created_by = excluded.created_by, created_at = excluded.created_at",
                        params![b2, n2, s2, by, now_ms()],
                    )?;
                    Ok(())
                })
                .await?;
            app.set_runtime_alias(&bot.name, n, Some(spec.to_string()));
            say(app, bot, ib, &format!("✅ {n}{marker} = {spec}")).await;
        }
        "unalias" => {
            let Some(arg) = words.first() else {
                say(app, bot, ib, &with_marker("usage: /unalias <prefix$> (the empty prefix is `$`)", marker)).await;
                return Ok(());
            };
            let n = prefix_name(arg, marker).to_string();
            let (b2, n2) = (bot.name.clone(), n.clone());
            let removed = app
                .db
                .call(move |c| Ok(c.execute("DELETE FROM aliases WHERE bot = ?1 AND name = ?2", params![b2, n2])?))
                .await?;
            app.set_runtime_alias(&bot.name, &n, None);
            let text = match (removed > 0, app.cfg.aliases.iter().find(|a| a.name == n)) {
                (true, Some(a)) => format!("runtime prefix {n}{marker} removed; the config's {n}{marker} = {} remains", a.spec),
                (true, None) => format!("✅ {n}{marker} removed"),
                (false, Some(_)) => format!("{n}{marker} comes from the config file; change it there"),
                (false, None) => format!("there is no prefix {n}{marker}"),
            };
            say(app, bot, ib, &text).await;
        }
        "credits" => match app.openrouter.key_info().await {
            Ok(v) => send(app, bot, ib, "OpenRouter key", credits_blocks(&v)?).await,
            Err(e) => say(app, bot, ib, &format!("⚠️ {e:#}")).await,
        },
        _ => {}
    }
    Ok(())
}

/// `/models`: this bot's prefixes, in config order. `/models <text>`: the
/// prefixes whose name or spec contains the text, and the model ids that work
/// as prefixes (providers with `allow_unlisted_models`).
async fn cmd_models(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, query: &str) -> Result<()> {
    let Some(cfg) = app.bot_cfg(&bot.name) else { return Ok(()) };
    let marker = app.marker();
    let prefixes: Vec<(String, String)> = app
        .aliases_in_order(&bot.name)
        .into_iter()
        .filter(|(_, spec)| app.served_canonical(cfg, spec).is_some())
        .collect();
    if !query.is_empty() {
        let q = query.to_lowercase();
        let mut blocks = Vec::new();
        let matching: Vec<Vec<String>> = prefixes
            .iter()
            .filter(|(n, s)| n.to_lowercase().contains(&q) || s.to_lowercase().contains(&q))
            .map(|(n, s)| vec![format!("{n}{marker}"), s.clone()])
            .collect();
        if !matching.is_empty() {
            blocks.push(table(&["prefix", "spec"], matching));
        }
        let own: Vec<Vec<String>> = app
            .chat_prefixes_of(&bot.name, ib.chat_id)
            .into_iter()
            .filter(|(n, p)| n.contains(&q) || p.spec.to_lowercase().contains(&q))
            .map(|(n, p)| vec![format!("{n}{marker}"), p.spec.clone()])
            .collect();
        if !own.is_empty() {
            blocks.push(table(&["this chat's prefix", "spec"], own));
        }
        if app.unlisted_models_allowed("openrouter") {
            let ids: Vec<Vec<String>> = app
                .openrouter
                .catalog
                .search(query, 200)
                .iter()
                .filter(|m| cfg.serves(&format!("openrouter:{}", m.id)))
                .take(40)
                .map(|m| {
                    let effort = m.max_effort().unwrap_or("-").to_string();
                    vec![format!("{}{marker}", m.id), m.architecture.input_modalities.join(","), effort]
                })
                .collect();
            if !ids.is_empty() {
                blocks.push(table(&["model id", "input", "max effort"], ids));
            }
        }
        if blocks.is_empty() {
            blocks.push(RBlock::Paragraph(RText::plain(format!("Nothing matches `{query}`."))));
        }
        send(app, bot, ib, &format!("Models matching `{query}`"), blocks).await;
        return Ok(());
    }
    let rows: Vec<Vec<String>> = prefixes.into_iter().map(|(n, s)| vec![format!("{n}{marker}"), s]).collect();
    let mut blocks = vec![table(&["prefix", "spec"], rows)];
    let own: Vec<Vec<String>> = app
        .chat_prefixes_of(&bot.name, ib.chat_id)
        .into_iter()
        .filter(|(_, p)| app.served_canonical(cfg, &p.spec).is_some())
        .map(|(n, p)| vec![format!("{n}{marker}"), p.spec.clone()])
        .collect();
    if !own.is_empty() {
        blocks.push(table(&["this chat's prefix", "spec"], own));
    }
    let tail = match direct_ids(app) {
        Some(ids) => format!("{ids} works as a prefix too. Search: /models <text>."),
        None => "Search: /models <text>.".to_string(),
    };
    blocks.push(RBlock::Paragraph(RText::plain(tail.replace('`', ""))));
    send(app, bot, ib, "Models", blocks).await;
    Ok(())
}

async fn cmd_model(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, arg: &str) -> Result<()> {
    let Some(cfg) = app.bot_cfg(&bot.name) else { return Ok(()) };
    let marker = app.marker();
    let usage = format!("usage: /model <prefix>, e.g. /model c{marker} (the empty prefix is {marker})");
    if arg.is_empty() {
        say(app, bot, ib, &usage).await;
        return Ok(());
    }
    let token = trigger::parse_spec(prefix_name(arg, marker)).ok();
    let spec = match token {
        Some(t) => app.resolve_spec(&t, cfg, ib.chat_id, false).await.ok().flatten(),
        None => None,
    };
    let Some(spec) = spec else {
        say(app, bot, ib, &format!("unknown prefix `{arg}`. {usage}")).await;
        return Ok(());
    };
    if spec.provider != "openrouter" {
        let title = format!("{}{marker} → {}:{}{}", spec.label, spec.provider, spec.model, spec.options_syntax());
        send(app, bot, ib, &title, vec![RBlock::Paragraph(RText::plain("served directly by this provider"))]).await;
        return Ok(());
    }
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
    send(app, bot, ib, &format!("{}{marker} → {}{}", spec.label, spec.model, spec.options_syntax()), blocks).await;
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

/// `/raw`: one message with the original Markdown (inline when short, else
/// an attached `.md`) and every original image/file of the answer attached.
async fn cmd_raw(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    let parts = &t.turn.parts;
    // Telegram turns tabs, CRs and other control characters into spaces: then only a file is exact.
    let altered = |s: &str| s.chars().any(|c| (c as u32) < 0x20 && c != '\n' || matches!(c, '\u{2028}' | '\u{2029}'));
    let (mut reasoning_len, mut answer_len, mut any_altered) = (0usize, 0usize, false);
    for p in parts {
        let (text, is_reasoning) = match p {
            Part::Reasoning(r) => (r.text.as_deref().unwrap_or(""), true),
            Part::Text { text, .. } | Part::Refusal { text } => (text.as_str(), false),
            _ => continue,
        };
        any_altered |= altered(text);
        if is_reasoning { reasoning_len += text.chars().count() } else { answer_len += text.chars().count() }
    }
    let inline = answer_len <= RAW_INLINE_MAX && reasoning_len <= 28_000 && !any_altered;
    let mut blocks = Vec::new();
    let mut sources = Vec::new();
    let mut files = 0;
    let mut file_block = |p: &Part, blocks: &mut Vec<RBlock>, sources: &mut Vec<Source>| {
        if let Part::Image { blob, filename, .. } | Part::File { blob, filename, .. } = p {
            files += 1;
            let ext = blob.mime.rsplit('/').next().unwrap_or("bin");
            let name = filename.clone().unwrap_or_else(|| format!("file-{}-{files}.{ext}", t.id));
            let key = format!("{}/{name}", blob.sha256);
            blocks.push(RBlock::Document { key: key.clone(), caption: RText::plain("") });
            sources.push(Source { key, blob: blob.clone(), filename: name, want: Want::Document });
        }
    };
    if inline {
        // In the order the model produced it; adjacent parts of one kind together.
        let mut i = 0;
        while i < parts.len() {
            match &parts[i] {
                Part::Reasoning(_) => {
                    let mut text = String::new();
                    while let Some(Part::Reasoning(r)) = parts.get(i) {
                        if let Some(t) = r.text.as_deref().filter(|t| !t.is_empty()) {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                        i += 1;
                    }
                    if !text.is_empty() {
                        blocks.push(RBlock::Details {
                            open: false,
                            title: RText::plain("💭 Reasoning (raw)"),
                            blocks: vec![RBlock::Pre { text, language: "markdown".into() }],
                        });
                    }
                }
                Part::Text { .. } | Part::Refusal { .. } => {
                    let mut text = String::new();
                    while let Some(Part::Text { text: t, .. } | Part::Refusal { text: t }) = parts.get(i) {
                        text.push_str(t);
                        i += 1;
                    }
                    if !text.is_empty() {
                        blocks.push(RBlock::Pre { text, language: "markdown".into() });
                    }
                }
                p => {
                    file_block(p, &mut blocks, &mut sources);
                    i += 1;
                }
            }
        }
    } else {
        let name = format!("answer-{}.md", t.id);
        let md = crate::compose::raw_markdown(parts);
        let blob = app.blobs.put(md.into_bytes(), "text/markdown".into()).await?;
        let key = format!("{}/{name}", blob.sha256);
        blocks.push(RBlock::Document { key: key.clone(), caption: RText::plain("") });
        sources.push(Source { key, blob, filename: name, want: Want::Document });
        for p in parts {
            file_block(p, &mut blocks, &mut sources);
        }
    }
    if blocks.is_empty() {
        say(app, bot, ib, "this answer is empty").await;
        return Ok(());
    }
    if let Err(e) = reply::send_blocks_with_media(app, bot, ib.chat_id, ib.msg_id, blocks, &sources).await {
        say(app, bot, ib, &format!("⚠️ {e:#}")).await;
    }
    Ok(())
}

/// `/dump`: the whole conversation up to the replied message as one JSON file:
/// every turn of the chain in order, as stored (all parts: text, reasoning with
/// signatures, tool calls, images, files; the provider's exact output), with
/// its model and options, status and attempts (outcome, usage, error), and
/// every image or file embedded once as base64. Request bodies: /info.
async fn cmd_dump(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    use base64::Engine;
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    let chain = conv::history(&app.db, t.id).await?;
    let iso = |ms: i64| jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default();
    let mut turns = Vec::new();
    let mut blobs = std::collections::BTreeMap::new();
    for row in &chain {
        let mut v = json!({
            "id": row.id,
            "parent_id": row.parent_id,
            "role": row.turn.role,
            "origin": row.turn.origin,
            "status": row.status.as_str(),
            "created_at": iso(row.created_at),
            "user_id": row.user_id,
        });
        if let Some(s) = &row.spec {
            v["model"] = json!({
                "provider": s.provider,
                "model": s.model,
                "options": s.options.iter().map(|o| o.to_syntax()).collect::<Vec<_>>(),
                "typed": s.raw,
            });
        }
        v["parts"] = serde_json::to_value(&row.turn.parts)?;
        if let Some(n) = &row.turn.native {
            v["native"] = serde_json::to_value(n)?;
        }
        if row.turn.role == omni_core::content::Role::Assistant {
            let mut attempts = Vec::new();
            for a in conv::attempts_of(&app.db, row.id).await? {
                let (_request, _output, error, upstream_id, usage) = conv::attempt_json(&app.db, a.id).await?;
                attempts.push(json!({
                    "n": a.n, "status": a.status, "upstream_id": upstream_id, "usage": usage, "error": error,
                }));
            }
            v["attempts"] = json!(attempts);
        }
        collect_blobs(&v, &mut blobs);
        turns.push(v);
    }
    let mut media = serde_json::Map::new();
    for (sha, r) in &blobs {
        let entry = match app.blobs.get(r).await {
            Ok(bytes) => json!({
                "mime": r.mime, "size": r.size, "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
            }),
            Err(e) => json!({"mime": r.mime, "size": r.size, "error": format!("{e:#}")}),
        };
        media.insert(sha.clone(), entry);
    }
    let n_turns = turns.len();
    let dump = json!({
        "format": "omni-conversation/1",
        "bot": bot.name,
        "chat_id": ib.chat_id,
        "exported_at": jiff::Timestamp::now().to_string(),
        "turns": turns,
        "blobs": media,
    });
    let name = format!("conversation-{}.json", t.id);
    let blob = app.blobs.put(serde_json::to_vec_pretty(&dump)?, "application/json".into()).await?;
    let key = format!("{}/{name}", blob.sha256);
    let blocks = vec![
        RBlock::Paragraph(RText::concat(vec![RText::plain("🤖 "), label("Conversation")])),
        RBlock::Paragraph(RText::plain(format!(
            "{n_turns} turns · {} images/files embedded · {}",
            blobs.len(),
            crate::compose::fmt_bytes(blob.size)
        ))),
        RBlock::Document { key: key.clone(), caption: RText::plain("") },
    ];
    let sources = [Source { key, blob, filename: name, want: Want::Document }];
    if let Err(e) = reply::send_blocks_with_media(app, bot, ib.chat_id, ib.msg_id, blocks, &sources).await {
        say(app, bot, ib, &format!("⚠️ {e:#}")).await;
    }
    Ok(())
}

/// Every blob reference (`{"sha256", "size", "mime"}`) anywhere in a value.
fn collect_blobs(v: &serde_json::Value, out: &mut std::collections::BTreeMap<String, omni_core::blob::BlobRef>) {
    match v {
        serde_json::Value::Object(m) => {
            if let Ok(r) = serde_json::from_value::<omni_core::blob::BlobRef>(v.clone())
                && m.len() == 3
                && r.sha256.len() == 64
            {
                out.insert(r.sha256.clone(), r);
                return;
            }
            m.values().for_each(|x| collect_blobs(x, out));
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| collect_blobs(x, out)),
        _ => {}
    }
}

/// A person's display name with id, from what the bot has seen.
async fn user_label(app: &App, id: i64) -> String {
    let name: Option<String> = app
        .db
        .call(move |c| {
            use rusqlite::OptionalExtension;
            Ok(c.query_row(
                "SELECT NULLIF(TRIM(COALESCE(first_name,'') || ' ' || COALESCE(last_name,'')), '') FROM users WHERE user_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
        })
        .await
        .ok()
        .flatten();
    match name {
        Some(n) => format!("{n} ({id})"),
        None => id.to_string(),
    }
}

/// `/prefix`: this chat's own prefixes (custom chatbots). Without arguments
/// the list; `/prefix <name>` shows one, or, as a reply to one of the bot's
/// answers, turns that conversation into a new one; `/prefix <name> <spec>$…`
/// creates one from marker lines. Administrators in their private chat may
/// list another chat's with `/prefix <chat_id>`.
async fn cmd_prefix(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    ib: &Inbound,
    args: &str,
    access: Access,
    admin: bool,
) -> Result<()> {
    use crate::prefixes;
    let Some(cfg) = app.bot_cfg(&bot.name) else { return Ok(()) };
    let marker = app.marker();
    let user = ib.sender_id.unwrap_or(0);
    let args = args.trim_start();
    let (first, rest) = match args.split_once(char::is_whitespace) {
        Some((f, r)) => (f, r.trim_start()),
        None => (args, ""),
    };
    let mut chat = ib.chat_id;
    if first.is_empty() || admin && chat > 0 && rest.is_empty() && first.parse::<i64>().is_ok_and(|c| c != 0) {
        if !first.is_empty() {
            chat = first.parse().unwrap_or(chat);
        }
        let mut rows = Vec::new();
        for (n, p) in app.chat_prefixes_of(&bot.name, chat) {
            let history = p.turns.iter().filter(|t| t.role != omni_core::content::Role::System).count();
            let system = if p.turns.iter().any(|t| t.role == omni_core::content::Role::System) { "yes" } else { "-" };
            rows.push(vec![format!("{n}{marker}"), p.spec.clone(), system.into(), history.to_string(), user_label(app, p.created_by).await]);
        }
        let blocks = if rows.is_empty() {
            vec![RBlock::Paragraph(RText::plain(with_marker(
                "No prefixes in this chat yet. Create one: /prefix name prefix$::system …",
                marker,
            )))]
        } else {
            vec![table(&["prefix", "spec", "system", "history", "by"], rows)]
        };
        let title = if chat == ib.chat_id { "Prefixes of this chat".to_string() } else { format!("Prefixes of {}", place_label(chat)) };
        send(app, bot, ib, &title, blocks).await;
        return Ok(());
    }
    let name = match prefixes::check_name(prefix_name(first, marker)) {
        Ok(n) => n,
        Err(e) => {
            say(app, bot, ib, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    // Show one.
    if rest.is_empty() && ib.reply_to_msg_id.is_none() {
        let Some(p) = app.chat_prefix(&bot.name, chat, &name) else {
            say(app, bot, ib, &format!("there is no prefix {name}{marker} in this chat")).await;
            return Ok(());
        };
        let mut blocks = vec![RBlock::Paragraph(RText::plain(format!(
            "{name}{marker} → {} · by {}",
            p.spec,
            user_label(app, p.created_by).await
        )))];
        for t in p.turns.iter() {
            let text: String = t
                .parts
                .iter()
                .map(|x| match x {
                    Part::Text { text, .. } => text.clone(),
                    Part::Image { .. } => "[image]".into(),
                    Part::File { .. } => "[file]".into(),
                    _ => String::new(),
                })
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(RBlock::Details {
                open: false,
                title: RText::plain(format!("::{}", t.role.as_str())),
                blocks: vec![RBlock::Pre { text, language: String::new() }],
            });
        }
        send(app, bot, ib, "Prefix", blocks).await;
        return Ok(());
    }
    // Create one: members allowed here and not banned.
    if access == Access::Banned {
        say(app, bot, ib, BANNED).await;
        return Ok(());
    }
    if app.prefix_taken(&bot.name, chat, &name) {
        say(app, bot, ib, &format!("⚠️ {name}{marker} is taken: pick another name")).await;
        return Ok(());
    }
    let (spec, turns) = if rest.is_empty() {
        // From the conversation up to the replied answer.
        let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
        if t.turn.role != omni_core::content::Role::Assistant || t.status != TurnStatus::Done {
            say(app, bot, ib, "⚠️ reply to an answer that finished normally").await;
            return Ok(());
        }
        let Some(spec) = t.spec.clone() else { return Ok(()) };
        let chain = conv::history(&app.db, t.id).await?;
        match prefixes::turns_from_chain(&chain) {
            Ok(turns) => (spec, turns),
            Err(e) => {
                say(app, bot, ib, &format!("⚠️ {e}")).await;
                return Ok(());
            }
        }
    } else {
        // From marker lines after a model prefix.
        let usage = with_marker(
            "usage: /prefix name prefix$::system\n…\n::user\n…\n::assistant\n… (the history is optional; it ends with ::assistant)",
            marker,
        );
        let t = match trigger::parse_trigger(rest, marker) {
            Ok(Some(t)) if t.specs.len() == 1 => t,
            Ok(_) => {
                say(app, bot, ib, &usage).await;
                return Ok(());
            }
            Err(e) => {
                say(app, bot, ib, &format!("⚠️ {e}")).await;
                return Ok(());
            }
        };
        let spec = match app.resolve_spec(&t.specs[0], cfg, chat, false).await {
            Ok(Some(s)) if s.template.is_none() => s,
            Ok(Some(_)) => {
                say(app, bot, ib, "⚠️ a prefix of this chat cannot be built on another one; use a model prefix").await;
                return Ok(());
            }
            Ok(None) => {
                say(app, bot, ib, &format!("⚠️ unknown prefix `{}{marker}`", t.specs[0].raw)).await;
                return Ok(());
            }
            Err(e) => {
                say(app, bot, ib, &format!("⚠️ {e}")).await;
                return Ok(());
            }
        };
        match prefixes::turns_from_text(&t.text) {
            Ok(turns) => (spec, turns),
            Err(e) => {
                say(app, bot, ib, &format!("⚠️ {e}. {usage}")).await;
                return Ok(());
            }
        }
    };
    // Options are validated when the prefix is used, like any other prefix's.
    let p = prefixes::ChatPrefix {
        spec: format!("{}{}", spec.canonical(), spec.options_syntax()),
        turns: Arc::new(turns),
        created_by: user,
        created_at: now_ms(),
    };
    if !prefixes::insert(&app.db, &bot.name, chat, &name, &p).await? {
        say(app, bot, ib, &format!("⚠️ {name}{marker} is taken: pick another name")).await;
        return Ok(());
    }
    let (history, system) = (
        p.turns.iter().filter(|t| t.role != omni_core::content::Role::System).count(),
        p.turns.iter().any(|t| t.role == omni_core::content::Role::System),
    );
    let text = format!(
        "✅ {name}{marker} → {} · {} · {history} history turns. Anyone in this chat can use it; /unprefix {name} deletes it.",
        p.spec,
        if system { "with a system prompt" } else { "no system prompt" },
    );
    app.chat_prefixes.write().entry((bot.name.clone(), chat)).or_default().insert(name, Arc::new(p));
    say(app, bot, ib, &text).await;
    Ok(())
}

/// `/unprefix <name>`: delete a prefix of this chat (its creator, or an
/// administrator; in their private chat also `/unprefix <chat_id> <name>`).
/// Replies to its earlier answers keep working: they use the model directly.
async fn cmd_unprefix(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, args: &str, admin: bool) -> Result<()> {
    use crate::prefixes;
    let marker = app.marker();
    let user = ib.sender_id.unwrap_or(0);
    let words: Vec<&str> = args.split_whitespace().collect();
    let (chat, name) = match words[..] {
        [n] => (ib.chat_id, n),
        [c, n] if admin && ib.chat_id > 0 && c.parse::<i64>().is_ok() => (c.parse().unwrap_or(0), n),
        _ => {
            say(app, bot, ib, "usage: /unprefix <name>").await;
            return Ok(());
        }
    };
    let name = prefix_name(name, marker).to_ascii_lowercase();
    let Some(p) = app.chat_prefix(&bot.name, chat, &name) else {
        say(app, bot, ib, &format!("there is no prefix {name}{marker} there")).await;
        return Ok(());
    };
    if p.created_by != user && !admin {
        say(app, bot, ib, "⛔ only its creator or an administrator can delete it").await;
        return Ok(());
    }
    prefixes::delete(&app.db, &bot.name, chat, &name).await?;
    if let Some(m) = app.chat_prefixes.write().get_mut(&(bot.name.clone(), chat)) {
        m.remove(&name);
    }
    say(app, bot, ib, &format!("✅ {name}{marker} deleted; replies to its earlier answers keep working")).await;
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
    // One message: the summary and the full request/response dump attached.
    let name = format!("request-{}.json", t.id);
    let blob = app.blobs.put(serde_json::to_vec_pretty(&dump)?, "application/json".into()).await?;
    let key = format!("{}/{name}", blob.sha256);
    blocks.insert(0, RBlock::Paragraph(RText::concat(vec![RText::plain("🤖 "), label("Request details")])));
    blocks.push(RBlock::Document { key: key.clone(), caption: RText::plain("") });
    let sources = [Source { key, blob, filename: name, want: Want::Document }];
    if let Err(e) = reply::send_blocks_with_media(app, bot, ib.chat_id, ib.msg_id, blocks, &sources).await {
        say(app, bot, ib, &format!("⚠️ {e:#}")).await;
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
    if !retry_turn(app, t.id).await? {
        say(app, bot, ib, "this answer is already being retried").await;
    }
    Ok(())
}

/// The "🔁 Retry" button under a reply that finally failed (callback data
/// `retry:<turn id>`). Anyone allowed to use the bot in the chat may press it,
/// like replying `/retry`; the retry runs (and is charged) as the original request.
pub async fn on_callback(app: &Arc<App>, bot: &Arc<BotConn>, q: tl::types::UpdateBotCallbackQuery) -> Result<()> {
    let chat_id = crate::tg::convert::marked_peer_id(&q.peer);
    let answer = |text: Option<&str>| {
        let call = tl::functions::messages::SetBotCallbackAnswer {
            alert: text.is_some(),
            query_id: q.query_id,
            message: text.map(str::to_string),
            url: None,
            cache_time: 0,
        };
        let bot = bot.clone();
        async move {
            if let Err(e) = bot.invoke(&call).await {
                tracing::warn!(error = %e, "answering a button press failed");
            }
        }
    };
    let data = q.data.as_deref().and_then(|d| std::str::from_utf8(d).ok()).unwrap_or_default();
    let Some(turn_id) = data.strip_prefix("retry:").and_then(|s| s.parse::<i64>().ok()) else {
        answer(None).await;
        return Ok(());
    };
    // The button must belong to a message of that reply, in this chat, sent by this bot.
    let shown = conv::turn_for_message(&app.db, &bot.name, chat_id, q.msg_id).await?.map(|(t, _)| t);
    if shown != Some(turn_id) {
        answer(Some("This button does not belong to this message.")).await;
        return Ok(());
    }
    // A retry is a paid request.
    match access::check(app, &bot.name, chat_id, q.user_id).await? {
        Access::Allowed => {}
        Access::Banned => {
            answer(Some("You are banned from using this bot in this chat.")).await;
            return Ok(());
        }
        Access::NotAllowed => {
            answer(None).await;
            return Ok(());
        }
    }
    let t = conv::get_turn(&app.db, turn_id).await?;
    if t.status != TurnStatus::Failed {
        answer(Some("This answer is not failed (any more).")).await;
        return Ok(());
    }
    if retry_turn(app, turn_id).await? {
        answer(None).await;
    } else {
        answer(Some("Someone else just retried it.")).await;
    }
    Ok(())
}

/// Generate a failed reply again, in the same messages (earlier attempts stay
/// visible above). `false` if someone else claimed the retry first.
async fn retry_turn(app: &Arc<App>, id: i64) -> Result<bool> {
    if !conv::claim_retry(&app.db, id).await? {
        return Ok(false);
    }
    jobs::spawn_turn(app.clone(), id);
    Ok(true)
}

async fn cmd_continue(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if t.status != TurnStatus::Done || !jobs::can_continue(app, t.id).await? {
        say(
            app,
            bot,
            ib,
            "only answers that stopped at the long-decode output budget can be continued (their note says \
             \"reached the long-decode output budget\" and suggests /continue)",
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
        say(app, bot, ib, &with_marker("reply to one of my messages: /m prefix$text", app.marker())).await;
        return Ok(());
    }
    let t = match trigger::parse_trigger(args, app.marker()) {
        Ok(Some(t)) => t,
        Ok(None) => {
            say(app, bot, ib, &with_marker("usage: /m prefix$text (e.g. /m g+s$what do you think?)", app.marker()))
                .await;
            return Ok(());
        }
        Err(e) => {
            say(app, bot, ib, &format!("⚠️ {e}")).await;
            return Ok(());
        }
    };
    let specs = match crate::handler::resolve_specs(app, bot, ib.chat_id, &t.specs, true).await {
        Ok(s) if s.iter().any(|x| x.template.is_some()) => {
            let msg = "⚠️ a prefix of this chat brings its own history, so it starts new conversations only; \
                       /m continues with a model prefix";
            say(app, bot, ib, msg).await;
            return Ok(());
        }
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

/// The start of a stats period in calendar days of `general.timezone`: `today`
/// is since 00:00 local time, `7d` and `30d` include today, `all` everything
/// (not rolling 24-hour windows). Also how the period reads, with its start.
fn period_start(app: &App, p: &str) -> (i64, String) {
    let tz_name = &app.cfg.general.timezone;
    let tz = jiff::tz::TimeZone::get(tz_name).unwrap_or(jiff::tz::TimeZone::UTC);
    let (days, label) = match p {
        "today" => (0, "today"),
        "7d" => (6, "7 days"),
        "all" => return (0, "all time".into()),
        _ => (29, "30 days"),
    };
    let start = jiff::Timestamp::now()
        .to_zoned(tz)
        .start_of_day()
        .and_then(|d| d.checked_sub(jiff::Span::new().days(days)))
        .ok();
    match start {
        Some(s) => (s.timestamp().as_millisecond(), format!("{label} (since {} {tz_name})", s.strftime("%Y-%m-%d %H:%M"))),
        None => (0, "all time".into()),
    }
}

/// A ranking dimension of `/stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dim {
    Model,
    Chat,
    User,
    ChatUser,
}

impl Dim {
    fn parse(w: &str) -> Option<Dim> {
        Some(match w {
            "model" => Dim::Model,
            "chat" => Dim::Chat,
            "user" => Dim::User,
            "chat-user" => Dim::ChatUser,
            _ => return None,
        })
    }

    fn header(self) -> &'static str {
        match self {
            Dim::Model => "model",
            Dim::Chat => "chat",
            Dim::User => "user",
            Dim::ChatUser => "chat · user",
        }
    }

    /// (label expression, GROUP BY expression) over `usage_ledger l`.
    fn sql(self) -> (String, &'static str) {
        let name = |id: &str| {
            format!(
                "COALESCE((SELECT NULLIF(TRIM(COALESCE(u.first_name,'') || ' ' || COALESCE(u.last_name,'')), '') \
                 FROM users u WHERE u.user_id = {id}), '?')"
            )
        };
        let chat = format!(
            "CASE WHEN l.chat_id > 0 THEN 'private · ' || {} ELSE COALESCE((SELECT c.title FROM chats c WHERE c.chat_id = l.chat_id), '?') END \
             || ' (' || l.chat_id || ')'",
            name("l.chat_id")
        );
        let user = format!("{} || ' (' || l.user_id || ')'", name("l.user_id"));
        match self {
            Dim::Model => ("l.model".into(), "l.model"),
            Dim::Chat => (chat, "l.chat_id"),
            Dim::User => (user, "l.user_id"),
            Dim::ChatUser => (format!("{chat} || ' · ' || {user}"), "l.chat_id, l.user_id"),
        }
    }
}

/// One ranking row: label, calls, input tokens, output tokens, cost.
type RankRow = (String, i64, i64, i64, f64);

/// Usage of a bot since `since` (optionally in one chat / of one person),
/// ranked by cost along `dim`, at most `limit` rows.
async fn rank(
    app: &App,
    bot: &str,
    since: i64,
    chat: Option<i64>,
    user: Option<i64>,
    dim: Dim,
    limit: usize,
) -> Result<Vec<RankRow>> {
    let (label, group) = dim.sql();
    let sql = format!(
        "SELECT {label}, COUNT(*), COALESCE(SUM(l.input_tokens),0), COALESCE(SUM(l.output_tokens),0), COALESCE(SUM(l.cost_usd),0)
         FROM usage_ledger l WHERE l.bot = ?1 AND l.ts >= ?2 AND (?3 IS NULL OR l.chat_id = ?3) AND (?4 IS NULL OR l.user_id = ?4)
         GROUP BY {group} ORDER BY 5 DESC LIMIT {limit}"
    );
    let bot = bot.to_string();
    app.db
        .call(move |c| {
            let mut s = c.prepare(&sql)?;
            let v = s
                .query_map(params![bot, since, chat, user], |r| {
                    Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(v)
        })
        .await
}

/// Total calls and cost of a bot since `since` (optionally in one chat / of one person).
async fn usage_total(app: &App, bot: &str, since: i64, chat: Option<i64>, user: Option<i64>) -> Result<(i64, f64)> {
    let bot = bot.to_string();
    app.db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(cost_usd),0) FROM usage_ledger
                 WHERE bot = ?1 AND ts >= ?2 AND (?3 IS NULL OR chat_id = ?3) AND (?4 IS NULL OR user_id = ?4)",
                params![bot, since, chat, user],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
}

/// `/stats [period]`, along the tree bot → chat → person: in a chat, that chat
/// (in a group also one member: `/stats <user_id>` or as a reply); in an
/// administrator's private chat the whole bot ranked by model, chat, person and
/// (chat, person) — or one ranking with `model|chat|user|chat-user` —,
/// `<chat_id>` one chat, or `<chat_id> <user_id>` one person in it. Always this
/// bot's usage only.
async fn cmd_stats(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, args: &str, admin: bool) -> Result<()> {
    let mut words = args.split_whitespace().collect::<Vec<_>>();
    let chat = ib.chat_id;
    let take_id = |words: &mut Vec<&str>| -> Option<i64> {
        let i = words.iter().position(|w| w.parse::<i64>().is_ok())?;
        words.remove(i).parse().ok()
    };
    // Scope (chat filter, user filter, description) and the rankings to show.
    let mut dims = vec![Dim::Model];
    let mut limit = 30;
    let (chat_f, user_f, scope_desc): (Option<i64>, Option<i64>, String) = if chat < 0 {
        let member = match take_id(&mut words) {
            Some(u) => Some(u),
            None if words.is_empty() && ib.reply_to_msg_id.is_some() => replied_sender(app, bot, ib).await,
            None => None,
        };
        match member {
            Some(u) => (Some(chat), Some(u), format!("user {u} in this group")),
            None => {
                dims.push(Dim::User);
                (Some(chat), None, "this group".into())
            }
        }
    } else if admin {
        // An administrator's private chat: the whole bot (rankings), one chat (a
        // private chat's id is its user's), or one person in one chat.
        let mut ids = Vec::new();
        while let Some(id) = take_id(&mut words) {
            ids.push(id);
        }
        let dim = words.iter().position(|w| Dim::parse(w).is_some()).map(|i| Dim::parse(words.remove(i)).unwrap());
        match ids[..] {
            [] => {
                match dim {
                    Some(d) => {
                        dims = vec![d];
                        limit = 50;
                    }
                    None => {
                        dims = vec![Dim::Model, Dim::Chat, Dim::User, Dim::ChatUser];
                        limit = 10;
                    }
                }
                (None, None, "every chat".into())
            }
            [c] => {
                dims.push(Dim::User);
                (Some(c), None, place_label(c))
            }
            [c, u, ..] => (Some(c), Some(u), format!("user {u} in {}", place_label(c))),
        }
    } else {
        (Some(chat), None, "this chat".into())
    };
    let (since, period) = period_start(app, words.first().copied().unwrap_or("30d"));
    let (calls, total) = usage_total(app, &bot.name, since, chat_f, user_f).await?;
    let mut blocks = vec![RBlock::Paragraph(RText::plain(format!(
        "{scope_desc} · {period} · {calls} calls · total {}",
        fmt_cost(total)
    )))];
    if calls == 0 {
        blocks.push(RBlock::Paragraph(RText::plain("No usage in this period.")));
    } else {
        for d in dims {
            let rows = rank(app, &bot.name, since, chat_f, user_f, d, limit).await?;
            let rows = rows
                .into_iter()
                .map(|(k, n, i, o, c)| vec![k, n.to_string(), i.to_string(), o.to_string(), fmt_cost(c)])
                .collect();
            blocks.push(table(&[d.header(), "calls", "in", "out", "cost"], rows));
        }
    }
    send(app, bot, ib, "Usage stats", blocks).await;
    Ok(())
}

/// `/usage`: this chat's spend limit and your own limit in this chat.
async fn cmd_usage(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let user = ib.sender_id.unwrap_or(0);
    let mut lines = Vec::new();
    for (what, target) in [("this chat", access::Target::Chat), ("you here", access::Target::Member(user))] {
        lines.push(match access::level(&app.db, &bot.name, ib.chat_id, target).await? {
            Some((lvl, r)) => format!(
                "{what}: balance {} of {} USD, refills {} USD/day",
                fmt_cost(lvl).trim_start_matches('$'),
                r.capacity,
                r.refill_per_day
            ),
            None => format!("{what}: no limit"),
        });
    }
    let items =
        lines.into_iter().map(|l| RItem { checkbox: None, blocks: vec![RBlock::Paragraph(RText::plain(l))] }).collect();
    send(app, bot, ib, "Spend limits", vec![RBlock::List { items }]).await;
    Ok(())
}

/// `/quota`: spend limits, which live inside one chat: the chat as a whole
/// (`chat`), each member by default (`members`), or one member (`<user_id>`,
/// or as a reply). In a group it acts on that group; in an administrator's
/// private chat the chat id comes first. Without numbers it shows them.
async fn cmd_quota(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, w: &[&str], by: i64) -> Result<()> {
    use access::Target;
    let group = ib.chat_id < 0;
    let usage = if group {
        "usage in a group: /quota [chat|members|<user_id> [<capacity_usd> <refill_usd_per_day> | off]]; as a reply: /quota [<capacity_usd> <refill_usd_per_day> | off] for that person"
    } else {
        "usage: /quota [<chat_id> [chat|members|<user_id> [<capacity_usd> <refill_usd_per_day> | off]]]"
    };
    let mut w: Vec<&str> = w.to_vec();
    let chat = if group {
        ib.chat_id
    } else {
        match w.first().map(|x| x.parse::<i64>()) {
            None => {
                send(app, bot, ib, "Spend limits", quota_blocks(app, &bot.name, None).await?).await;
                return Ok(());
            }
            Some(Ok(c)) if c != 0 => {
                w.remove(0);
                c
            }
            Some(_) => {
                say(app, bot, ib, usage).await;
                return Ok(());
            }
        }
    };
    // The target: a keyword, a user id, the replied person, or (none) the whole list.
    let target = match w.first().copied() {
        Some("chat") => Some(Target::Chat),
        Some("members") => Some(Target::Members),
        Some(x) if x.parse::<i64>().is_ok_and(|u| u > 0) && w.len() != 2 => Some(Target::Member(x.parse().unwrap())),
        _ => None,
    };
    if target.is_some() {
        w.remove(0);
    }
    let target = match target {
        Some(t) => t,
        None if w.is_empty() && !group => {
            send(app, bot, ib, "Spend limits", quota_blocks(app, &bot.name, Some(chat)).await?).await;
            return Ok(());
        }
        None if w.is_empty() && ib.reply_to_msg_id.is_none() => {
            send(app, bot, ib, "Spend limits of this group", quota_blocks(app, &bot.name, Some(chat)).await?).await;
            return Ok(());
        }
        None => match replied_sender(app, bot, ib).await {
            Some(u) if group => Target::Member(u),
            _ => {
                say(app, bot, ib, usage).await;
                return Ok(());
            }
        },
    };
    let who = match target {
        Target::Chat if group => "this group".to_string(),
        Target::Chat => place_label(chat),
        Target::Members if group => "each member of this group".to_string(),
        Target::Members => format!("each member of {}", place_label(chat)),
        Target::Member(u) if group => format!("user {u} in this group"),
        Target::Member(u) => format!("user {u} in {}", place_label(chat)),
    };
    let rule = match (w.first(), w.get(1)) {
        (None, None) => {
            // Show it.
            let text = match (target, access::level(&app.db, &bot.name, chat, target).await?) {
                (Target::Members, _) => match access::rule_for(&app.db, &bot.name, chat, Target::Members).await? {
                    Some(r) => format!("{who}: {} USD capacity, refills {} USD/day", r.capacity, r.refill_per_day),
                    None => format!("{who}: no limit"),
                },
                (_, Some((lvl, r))) => {
                    let default = match target {
                        Target::Member(_) if !access::has_own_rule(&app.db, &bot.name, chat, target).await? => {
                            " (the default for members)"
                        }
                        _ => "",
                    };
                    format!(
                        "{who}: balance {} of {} USD, refills {} USD/day{default}",
                        fmt_cost(lvl).trim_start_matches('$'),
                        r.capacity,
                        r.refill_per_day
                    )
                }
                (_, None) => format!("{who}: no limit"),
            };
            say(app, bot, ib, &text).await;
            return Ok(());
        }
        (Some(&"off"), None) => None,
        (Some(c), Some(r)) if w.len() == 2 => match (c.parse::<f64>(), r.parse::<f64>()) {
            (Ok(c), Ok(r)) if c >= 0.0 && r >= 0.0 => Some(access::Rule { capacity: c, refill_per_day: r }),
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
    access::set_rule(&app.db, &bot.name, chat, target, rule, by).await.context("set quota")?;
    let text = match rule {
        Some(r) => format!("✅ {who}: {} USD capacity, refills {} USD/day", r.capacity, r.refill_per_day),
        None => format!("✅ {who}: no limit"),
    };
    say(app, bot, ib, &text).await;
    Ok(())
}

/// The spend-limit rules of a bot (every chat, or one), with balances.
async fn quota_blocks(app: &App, bot: &str, only_chat: Option<i64>) -> Result<Vec<RBlock>> {
    use access::Target;
    let mut rows = Vec::new();
    for q in access::quota_rules(&app.db, bot, only_chat).await? {
        let (target, balance) = match q.target {
            Target::Chat => ("whole chat".to_string(), access::level(&app.db, bot, q.chat_id, q.target).await?),
            Target::Members => ("each member".to_string(), None),
            Target::Member(u) => (format!("user {u}"), access::level(&app.db, bot, q.chat_id, q.target).await?),
        };
        let balance = balance.map(|(l, _)| fmt_cost(l).trim_start_matches('$').to_string()).unwrap_or("-".into());
        let mut row = vec![target, format!("{}", q.rule.capacity), format!("{}", q.rule.refill_per_day), balance];
        if only_chat.is_none() {
            row.insert(0, place_label(q.chat_id));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Ok(vec![RBlock::Paragraph(RText::plain("No spend limits."))]);
    }
    let header: &[&str] = if only_chat.is_none() {
        &["chat", "applies to", "capacity", "refill/day", "balance"]
    } else {
        &["applies to", "capacity", "refill/day", "balance"]
    };
    Ok(vec![table(header, rows)])
}

