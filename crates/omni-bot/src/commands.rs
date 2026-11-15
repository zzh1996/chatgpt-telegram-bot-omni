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
use crate::compose::{fmt_cost, fmt_tokens_compact};
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
    let typed = |id: &str| app.provider_enabled(id) && app.unlisted_models_allowed(id);
    if typed("openrouter") {
        forms.push(format!("`author/model{m}` on OpenRouter"));
    }
    for p in omni_llm::provider::PROVIDERS.iter().filter(|p| typed(p.id)) {
        forms.push(format!("`{}:model{m}` on {}", p.id, p.name));
    }
    (!forms.is_empty()).then(|| forms.join(", "))
}

/// Commands that start requests that cost money. Every other non-admin command
/// only shows information: answered for everyone in an allowed chat, people
/// banned there included. Where the chat is not allowed only /ping and an
/// administrator's /allow are answered.
const PAID_COMMANDS: &[&str] = &["m", "retry", "continue"];

/// The answer to a paid request from someone banned in the chat.
pub fn banned(chat_id: i64) -> String {
    format!("⛔ You are banned from using this bot in {}.", access::this_chat(chat_id))
}

const HINT: &str = "Start a conversation with `prefix$question` (the prefixes are listed by /models), or reply to one of my answers to continue. /help for details.";

/// Commands only administrators may run: in their private chat for anything of
/// this bot, inside a group only those in [`GROUP_ADMIN_COMMANDS`], for that group.
const ADMIN_COMMANDS: &[&str] = &["allow", "deny", "ban", "unban", "acl", "quota", "alias", "unalias", "credits"];
/// Admin commands that work inside a group; they only see and change that group,
/// never anything about other chats.
const GROUP_ADMIN_COMMANDS: &[&str] = &["allow", "deny", "ban", "unban", "acl", "quota"];

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
        let place = access::this_chat(ib.chat_id);
        let here = match access {
            Access::Allowed => format!("{place} is allowed"),
            Access::Banned => format!("{place} is allowed, you are banned here"),
            Access::NotAllowed => format!("{place} is not allowed"),
        };
        let up = jiff::Timestamp::now().duration_since(app.started_at);
        say(
            app,
            bot,
            ib,
            &format!(
                "pong · chat_id={} · user_id={}{}{} · {here} · bot={} · up {}",
                ib.chat_id,
                user,
                ib.payload.sender_username.as_ref().map(|u| format!(" (@{u})")).unwrap_or_default(),
                if admin { " · admin" } else { "" },
                bot.name,
                fmt_uptime(up.as_secs().max(0) as u64)
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
    // `/tibo`: an easter egg the owner asked for (2026-11-16), deliberately in no
    // help, command menu or doc: like the Tibo who resets rate limits, it puts every
    // balance of this chat back to full. An admin's; anyone else gets no answer.
    if name == "tibo" {
        if admin {
            let text = match access::reset_all(&app.db, &bot.name, ib.chat_id).await? {
                0 => format!("🪄 Tibo looked, but {} has no limits to reset.", access::this_chat(ib.chat_id)),
                _ => format!(
                    "🪄 Tibo reset the limits: {} and everyone in it are back to a full balance.",
                    access::this_chat(ib.chat_id)
                ),
            };
            say(app, bot, ib, &text).await;
        }
        return Ok(());
    }
    // Anyone may look at this chat's spend limits (owner, 2026-11-16); changing
    // them, and other chats', is for admins.
    if name == "quota" && !admin {
        let words: Vec<&str> = args.split_whitespace().collect();
        return cmd_quota(app, bot, ib, &words, user, false).await;
    }
    if ADMIN_COMMANDS.contains(&name) {
        // In a group scoped to that group; everything else from the private chat.
        if !admin {
            say(app, bot, ib, "⛔ admin only").await;
        } else if ib.chat_id < 0 && !GROUP_ADMIN_COMMANDS.contains(&name) {
            say(app, bot, ib, &format!("⛔ /{name} works only in your private chat with me.")).await;
        } else {
            admin_command(app, bot, ib, name, args, user).await?;
        }
        return Ok(());
    }
    // Paid requests: not when banned here (queries are answered even then).
    if PAID_COMMANDS.contains(&name) && access == Access::Banned {
        say(app, bot, ib, &banned(ib.chat_id)).await;
        return Ok(());
    }
    match name {
        "start" | "help" => {
            let enabled_ids = |id: &str| app.provider_enabled(id) && app.unlisted_models_allowed(id);
            let ctx = crate::help::Ctx {
                marker: app.marker().to_string(),
                ids_openrouter: enabled_ids("openrouter"),
                ids_platforms: omni_llm::provider::PROVIDERS.iter().any(|p| enabled_ids(p.id)),
                max_file_mb: app.cfg.general.max_file_mb,
                bold_cost_usd: app.cfg.render.bold_cost_usd,
                answer_first: app.cfg.render.answer_first,
                admin,
                group: ib.chat_id < 0,
                lang: crate::help::Lang::En,
            };
            // `/start` may carry a deep-link payload: always the overview.
            let raw = if name == "start" { "" } else { args.trim() };
            // `/help_zh …` (or `/help zh …`): the same help in Chinese.
            let (ctx, topic) = match raw.strip_prefix("zh") {
                Some(rest) if rest.is_empty() || rest.starts_with(['_', ' ']) => {
                    (crate::help::Ctx { lang: crate::help::Lang::Zh, ..ctx }, rest.trim_start_matches(['_', ' ']))
                }
                _ => (ctx, raw),
            };
            let help = if ctx.lang == crate::help::Lang::Zh { "帮助" } else { "Help" };
            if topic.is_empty() {
                send(app, bot, ib, help, crate::help::overview(&ctx)).await;
            } else {
                match crate::help::topic(&ctx, topic) {
                    Ok((title, blocks)) => send(app, bot, ib, &format!("{help} · {title}"), blocks).await,
                    Err(e) => send(app, bot, ib, help, md(&e)).await,
                }
            }
        }
        "models" => cmd_models(app, bot, ib, args.trim()).await?,
        "model" => cmd_model(app, bot, ib, args.trim()).await?,
        "raw" => cmd_raw(app, bot, ib).await?,
        "full" => cmd_full(app, bot, ib).await?,
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

/// How long the bot has been running: `42s`, `5m 3s`, `3h 5m`, `10d 0h`.
fn fmt_uptime(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        3600..86400 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {}h", s / 86400, s % 86400 / 3600),
    }
}

/// A platform's balance (`/credits`): the amounts readable (DeepSeek's
/// `balance_infos`, Moonshot's `data`), the full response folded underneath.
fn balance_blocks(v: &serde_json::Value) -> Vec<RBlock> {
    use serde_json::Value;
    let s = |x: Option<&Value>| match x {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => "-".into(),
    };
    let mut lines = Vec::new();
    for b in v.get("balance_infos").and_then(Value::as_array).into_iter().flatten() {
        lines.push(format!(
            "{} {} (granted {}, topped up {})",
            s(b.get("currency")),
            s(b.get("total_balance")),
            s(b.get("granted_balance")),
            s(b.get("topped_up_balance"))
        ));
    }
    if let Some(d) = v.get("data").filter(|d| d.get("available_balance").is_some()) {
        lines.push(format!(
            "available {} (cash {}, vouchers {})",
            s(d.get("available_balance")),
            s(d.get("cash_balance")),
            s(d.get("voucher_balance"))
        ));
    }
    let mut out: Vec<RBlock> = Vec::new();
    if !lines.is_empty() {
        let items = lines
            .into_iter()
            .map(|l| RItem { checkbox: None, blocks: vec![RBlock::Paragraph(RText::plain(l))] })
            .collect();
        out.push(RBlock::List { items });
    }
    out.push(RBlock::Details {
        open: false,
        title: RText::plain("full response"),
        blocks: vec![RBlock::Pre {
            text: serde_json::to_string_pretty(v).unwrap_or_default(),
            language: "json".into(),
        }],
    });
    out
}

/// `/credits`: the OpenRouter key's limit and spending, readable, with the full
/// response folded underneath (without the key's masked label).
fn credits_blocks(v: &serde_json::Value) -> Result<Vec<RBlock>> {
    let d = v.get("data").unwrap_or(v);
    let num = |k: &str| d.get(k).and_then(serde_json::Value::as_f64);
    let usd = |x: Option<f64>| x.map(fmt_cost).unwrap_or_else(|| "-".into());
    let mut lines = Vec::new();
    // `limit` and `limit_remaining` are per period (`limit_reset`); `usage` is all time.
    let (per, this) = match d.get("limit_reset").and_then(|x| x.as_str()) {
        Some("daily") => (" a day".to_string(), " today"),
        Some("weekly") => (" a week".to_string(), " this week"),
        Some("monthly") => (" a month".to_string(), " this month"),
        Some(other) => (format!(" (resets {other})"), ""),
        None => (String::new(), ""),
    };
    match num("limit") {
        Some(limit) => lines.push(format!(
            "limit {}{per} · {} left{this} · {} used in total",
            usd(Some(limit)),
            usd(num("limit_remaining")),
            usd(num("usage"))
        )),
        None => lines.push(format!("no limit · {} used in total", usd(num("usage")))),
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
        && let (Some(used), Some(limit)) =
            (f.get("used").and_then(|x| x.as_i64()), f.get("limit").and_then(|x| x.as_i64()))
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
async fn admin_command(
    app: &Arc<App>,
    bot: &Arc<BotConn>,
    ib: &Inbound,
    name: &str,
    args: &str,
    by: i64,
) -> Result<()> {
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
                say(
                    app,
                    bot,
                    ib,
                    &format!("⛔ In a group, /{name} only acts on this group. Use private chat for others."),
                )
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
                    let names =
                        [r.place.filter(|_| !group), r.user].into_iter().flatten().collect::<Vec<_>>().join(" · ");
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
        "quota" => cmd_quota(app, bot, ib, &words, by, true).await?,
        "alias" => {
            let mut it = args.trim().splitn(2, char::is_whitespace);
            let (Some(n), Some(spec)) = (it.next(), it.next()) else {
                say(
                    app,
                    bot,
                    ib,
                    &with_marker(
                        "usage: /alias <prefix$> <spec>: the spec is the id of a model this bot has, with options, \
                         e.g. /alias low$ openai:gpt-6.1-sol+e=low (the empty prefix is `$`)",
                        marker,
                    ),
                )
                .await;
                return Ok(());
            };
            let n = prefix_name(n, marker);
            let spec = spec.trim();
            if spec.contains(',') {
                say(app, bot, ib, "⚠️ a prefix is one model (with options), not a list").await;
                return Ok(());
            }
            let parsed = match trigger::parse_spec(spec) {
                Ok(p) => p,
                Err(e) => {
                    say(app, bot, ib, &format!("⚠️ invalid spec: {e}")).await;
                    return Ok(());
                }
            };
            // Only a model this bot has: anything else would be a prefix that only errs.
            let has = app.bot_cfg(&bot.name).and_then(|cfg| app.served_canonical(cfg, spec)).is_some_and(|c| {
                match c.strip_prefix("openrouter:") {
                    Some(m) => app.openrouter.catalog.get(m).is_some(),
                    None => true,
                }
            });
            if !has {
                say(
                    app,
                    bot,
                    ib,
                    &format!(
                        "⚠️ not saved: this bot has no model `{}` (a model id with its platform, e.g. \
                         `openai:gpt-6.1-sol`, or an OpenRouter id `author/model`; /models lists what it has)",
                        parsed.model
                    ),
                )
                .await;
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
                (true, Some(a)) => {
                    format!("runtime prefix {n}{marker} removed; the config's {n}{marker} = {} remains", a.spec)
                }
                (true, None) => format!("✅ {n}{marker} removed"),
                (false, Some(_)) => format!("{n}{marker} comes from the config file; change it there"),
                (false, None) => format!("there is no prefix {n}{marker}"),
            };
            say(app, bot, ib, &text).await;
        }
        "credits" => {
            let mut blocks = Vec::new();
            if app.provider_enabled("openrouter") {
                blocks.push(RBlock::Paragraph(RText::bold(RText::plain("OpenRouter key"))));
                match app.openrouter.key_info().await {
                    Ok(v) => blocks.extend(credits_blocks(&v)?),
                    Err(e) => blocks.push(RBlock::Paragraph(RText::plain(format!("⚠️ {e:#}")))),
                }
            }
            // The platforms' own APIs that have a balance endpoint.
            let balances = futures::future::join_all(
                app.direct.values().map(|p| async move { p.balance().await.map(|b| (p.name(), b)) }),
            )
            .await;
            for (name, b) in balances.into_iter().flatten() {
                blocks.push(RBlock::Paragraph(RText::bold(RText::plain(format!("{name} balance")))));
                match b {
                    Ok(v) => blocks.extend(balance_blocks(&v)),
                    Err(e) => blocks.push(RBlock::Paragraph(RText::plain(format!("⚠️ {e:#}")))),
                }
            }
            if blocks.is_empty() {
                blocks.push(RBlock::Paragraph(RText::plain("no configured API reports a balance")));
            }
            send(app, bot, ib, "Credits", blocks).await
        }
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
            blocks.push(table(&["prefix", "model"], matching));
        }
        let own: Vec<Vec<String>> = app
            .chat_prefixes_of(&bot.name, ib.chat_id)
            .into_iter()
            .filter(|(n, p)| n.contains(&q) || p.spec.to_lowercase().contains(&q))
            .map(|(n, p)| vec![format!("{n}{marker}"), p.spec.clone()])
            .collect();
        if !own.is_empty() {
            blocks.push(table(&[&format!("{}'s prefix", access::this_chat(ib.chat_id)), "model"], own));
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
    let mut blocks = if rows.is_empty() {
        // Prefixes of platforms without an API key are not offered.
        vec![RBlock::Paragraph(RText::plain("No prefix of this bot is available (their platforms have no API key)."))]
    } else {
        vec![table(&["prefix", "model"], rows)]
    };
    let own: Vec<Vec<String>> = app
        .chat_prefixes_of(&bot.name, ib.chat_id)
        .into_iter()
        .filter(|(_, p)| app.served_canonical(cfg, &p.spec).is_some())
        .map(|(n, p)| vec![format!("{n}{marker}"), p.spec.clone()])
        .collect();
    if !own.is_empty() {
        blocks.push(table(&[&format!("{}'s prefix", access::this_chat(ib.chat_id)), "model"], own));
    }
    let tail = match direct_ids(app) {
        Some(ids) => format!("A model id ({ids}) works as a prefix too. Search: /models <text>."),
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
        Some(t) => match app.resolve_spec(&t, cfg, ib.chat_id, false).await {
            Ok(s) => s,
            Err(e) => {
                say(app, bot, ib, &format!("⚠️ {e}")).await;
                return Ok(());
            }
        },
        None => None,
    };
    let Some(spec) = spec else {
        say(app, bot, ib, &format!("unknown prefix `{arg}`. {usage}")).await;
        return Ok(());
    };
    if spec.provider != "openrouter" {
        let title = format!("{}{marker} → {}:{}{}", spec.label, spec.provider, spec.model, spec.options_syntax());
        let name = omni_llm::provider::info(&spec.provider).map(|i| i.name).unwrap_or(spec.provider.as_str());
        let text = format!("served by the {name} API directly");
        send(app, bot, ib, &title, vec![RBlock::Paragraph(RText::plain(text))]).await;
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
    send(app, bot, ib, &format!("{}{marker} → {}{}", spec.label, spec.canonical(), spec.options_syntax()), blocks)
        .await;
    Ok(())
}

/// The assistant turn a reply command targets.
async fn target_turn(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<Option<conv::TurnRow>> {
    let Some(rt) = ib.reply_to_msg_id else {
        say(app, bot, ib, "reply to one of my messages with this command").await;
        return Ok(None);
    };
    let Some((turn_id, _)) = conv::turn_for_message(&app.db, &bot.name, ib.chat_id, rt).await? else {
        let text = if conv::sent_by(&app.db, &bot.name, ib.chat_id, rt).await? {
            "that message is not a model's answer: reply to an answer with this command"
        } else {
            "that message is not one of my answers"
        };
        say(app, bot, ib, text).await;
        return Ok(None);
    };
    Ok(Some(conv::get_turn(&app.db, turn_id).await?))
}

/// The answer is still being generated (`/raw`, `/dump`… need it finished).
const STILL_GENERATING: &str = "this answer is still being generated; try again when it is finished";

fn generating(t: &conv::TurnRow) -> bool {
    matches!(t.status, TurnStatus::Pending | TurnStatus::Running)
}

/// Raw answers up to this size are sent inline (code block with a copy button), longer ones as a file.
const RAW_INLINE_MAX: usize = 2000;

/// `/raw`: one message with the original Markdown (inline when short, else
/// an attached `.md`) and every original image/file of the answer attached.
/// `/full`: the answer's reasoning and tool calls, complete, folded as in the
/// answer (which shortens them when they alone would make it longer than one
/// message), in one or more messages.
async fn cmd_full(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if generating(&t) {
        say(app, bot, ib, STILL_GENERATING).await;
        return Ok(());
    }
    let model = t.spec.as_ref().map(|s| s.canonical()).unwrap_or_default();
    let parts = &t.turn.parts;
    let Some(pages) = crate::compose::full_process(&model, parts) else {
        say(app, bot, ib, "this answer has no reasoning or tool calls").await;
        return Ok(());
    };
    // Images and files inside the folds (code tools, thought images).
    let keys: Vec<String> = pages.iter().flat_map(|p| omni_markdown::rich::media_keys(p)).collect();
    let mut sources: Vec<Source> = Vec::new();
    for p in parts {
        let (blob, name, want) = match p {
            Part::Image { blob, .. } => (blob, None, Want::Photo),
            Part::File { blob, filename, .. } => (blob, filename.clone(), Want::Document),
            _ => continue,
        };
        if keys.contains(&blob.sha256) && !sources.iter().any(|s| s.key == blob.sha256) {
            let ext = blob.mime.rsplit('/').next().unwrap_or("bin");
            sources.push(Source {
                key: blob.sha256.clone(),
                blob: blob.clone(),
                filename: name.unwrap_or_else(|| format!("file.{ext}")),
                want,
            });
        }
    }
    if let Err(e) = reply::send_pages_with_media(app, bot, ib.chat_id, ib.msg_id, pages, &sources).await {
        say(app, bot, ib, &format!("⚠️ {e:#}")).await;
    }
    Ok(())
}

async fn cmd_raw(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if generating(&t) {
        say(app, bot, ib, STILL_GENERATING).await;
        return Ok(());
    }
    if t.status == TurnStatus::Failed {
        let msg = "this answer failed, so it has no final text; each attempt's output and error is folded in the answer, the requests are in /info";
        say(app, bot, ib, msg).await;
        return Ok(());
    }
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
        let name = format!("answer-{}.md", crate::compose::file_stamp(&app.cfg.general.timezone, t.created_at));
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
/// How many bytes of images and files one /dump embeds (the dump is built in memory).
const DUMP_EMBED_BYTES: u64 = 64 << 20;

async fn cmd_dump(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    use base64::Engine;
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if generating(&t) {
        say(app, bot, ib, STILL_GENERATING).await;
        return Ok(());
    }
    let chain = conv::history(&app.db, t.id).await?;
    let iso = |ms: i64| jiff::Timestamp::from_millisecond(ms).map(|t| t.to_string()).unwrap_or_default();
    let mut turns = Vec::new();
    let mut blobs = std::collections::BTreeMap::new();
    // Turns by their place in this conversation (`n`, each the reply to the one
    // before): the database's global ids would tell how much the bot is used elsewhere.
    for (n, row) in chain.iter().enumerate() {
        let mut v = json!({
            "n": n,
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
        // The conversation's own images and files only: never an object that merely
        // looks like a file reference inside tool input or output, provider data or
        // errors, which models and providers control.
        for p in &row.turn.parts {
            if let Part::Image { blob, .. } | Part::File { blob, .. } = p
                && omni_core::blob::is_sha256_hex(&blob.sha256)
            {
                blobs.insert(blob.sha256.clone(), blob.clone());
            }
        }
        turns.push(v);
    }
    // Files are embedded up to DUMP_EMBED_BYTES in all (memory); the rest are listed.
    let mut media = serde_json::Map::new();
    let mut embedded = 0u64;
    for (sha, r) in &blobs {
        let entry = if embedded + r.size > DUMP_EMBED_BYTES {
            json!({"mime": r.mime, "size": r.size, "omitted": "too large to embed in one dump"})
        } else {
            match app.blobs.get(r).await {
                Ok(bytes) => {
                    embedded += bytes.len() as u64;
                    json!({
                        "mime": r.mime, "size": r.size, "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                    })
                }
                Err(e) => json!({"mime": r.mime, "size": r.size, "error": format!("{e:#}")}),
            }
        };
        media.insert(sha.clone(), entry);
    }
    let n_turns = turns.len();
    // The same conversation always dumps to the same bytes (no export time), so
    // asking again stores and uploads nothing new.
    let dump = json!({
        "format": "omni-conversation/2",
        "bot": bot.name,
        "chat_id": ib.chat_id,
        "turns": turns,
        "blobs": media,
    });
    let name = format!("conversation-{}.json", crate::compose::file_stamp(&app.cfg.general.timezone, t.created_at));
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
            rows.push(vec![
                format!("{n}{marker}"),
                p.spec.clone(),
                system.into(),
                history.to_string(),
                user_label(app, p.created_by).await,
            ]);
        }
        let blocks = if rows.is_empty() {
            vec![RBlock::Paragraph(RText::plain(with_marker(
                &format!("No prefixes in {} yet: /help prefixes shows how to make one.", access::this_chat(chat)),
                marker,
            )))]
        } else {
            vec![table(&["prefix", "model", "system prompt", "history turns", "by"], rows)]
        };
        let title = if chat == ib.chat_id {
            format!("Prefixes of {}", access::this_chat(chat))
        } else {
            format!("Prefixes of {}", place_label(chat))
        };
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
            say(app, bot, ib, &format!("there is no prefix {name}{marker} in {}", access::this_chat(chat))).await;
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
        say(app, bot, ib, &banned(ib.chat_id)).await;
        return Ok(());
    }
    if app.prefix_taken(&bot.name, chat, &name) {
        say(app, bot, ib, &format!("⚠️ {name}{marker} is taken: pick another name")).await;
        return Ok(());
    }
    let (spec, turns) = if rest.is_empty() {
        // From the conversation up to the replied answer.
        let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
        if generating(&t) {
            say(app, bot, ib, &format!("⚠️ {STILL_GENERATING}")).await;
            return Ok(());
        }
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
    match prefixes::insert(&app.db, &bot.name, chat, &name, &p).await? {
        prefixes::Inserted::Added => {}
        prefixes::Inserted::Taken => {
            say(app, bot, ib, &format!("⚠️ {name}{marker} is taken: pick another name")).await;
            return Ok(());
        }
        prefixes::Inserted::Full => {
            let here = if chat == ib.chat_id { access::this_chat(chat).to_string() } else { place_label(chat) };
            let max = prefixes::MAX_PER_CHAT;
            say(app, bot, ib, &format!("⚠️ {here} has {max} prefixes, the most a chat can have: /unprefix one first"))
                .await;
            return Ok(());
        }
    }
    let (history, system) = (
        p.turns.iter().filter(|t| t.role != omni_core::content::Role::System).count(),
        p.turns.iter().any(|t| t.role == omni_core::content::Role::System),
    );
    let text = format!(
        "✅ {name}{marker} → {} · {} · {history} history turns. Anyone in {} can use it; /unprefix {name} deletes it.",
        p.spec,
        if system { "with a system prompt" } else { "no system prompt" },
        access::this_chat(chat),
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
            "model: {}{} · status: {}",
            s.canonical(),
            Some(s.options_syntax()).filter(|o| !o.is_empty()).map(|o| format!(" · options: {o}")).unwrap_or_default(),
            format!("{:?}", t.status).to_lowercase()
        ))));
    }
    let mut dump = json!({"attempts": []});
    for a in &attempts {
        let (req, _out, err, up, usage) = conv::attempt_json(&app.db, a.id).await?;
        let meta = conv::attempt_meta(&app.db, a.id).await.ok().flatten();
        let mut line = format!("attempt {} · {}", a.n, a.status);
        if let Some(ms) = meta.as_ref().and_then(|m| m["first_output_ms"].as_u64()) {
            line.push_str(&format!(" · first output {:.1}s", ms as f64 / 1000.0));
        }
        if let Some(u) = up.as_deref().filter(|u| !u.is_empty()) {
            line.push_str(&format!(" · {u}"));
        }
        let calls: Vec<omni_llm::meta::CallMeta> =
            meta.as_ref().and_then(|m| serde_json::from_value(m["calls"].clone()).ok()).unwrap_or_default();
        let mut lines = vec![line];
        lines.extend(calls.iter().filter_map(call_line));
        blocks.push(RBlock::Paragraph(RText::plain(lines.join("\n"))));
        dump["attempts"].as_array_mut().unwrap().push(json!({
            "n": a.n, "status": a.status, "upstream_id": up, "usage": usage, "error": err, "request": req,
            "calls": meta.as_ref().map(|m| m["calls"].clone()),
        }));
    }
    // One message: the summary and the full request/response dump attached.
    let name = format!("request-{}.json", crate::compose::file_stamp(&app.cfg.general.timezone, t.created_at));
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

/// One upstream call in `/info`: its ids, how it was served, its timing and
/// rate limits (everything else is in the attached JSON).
fn call_line(c: &omni_llm::meta::CallMeta) -> Option<String> {
    let r = &c.response;
    let str_at = |path: &[&str]| {
        let mut v = r.get(path[0])?;
        for k in &path[1..] {
            v = v.get(k)?;
        }
        v.as_str().filter(|s| !s.is_empty()).map(str::to_string)
    };
    let mut f = Vec::new();
    if let Some(s) = c.status.filter(|s| *s != 200) {
        f.push(format!("HTTP {s}"));
    }
    let id = str_at(&["id"]).or_else(|| str_at(&["responseId"]));
    match (c.request_id(), id) {
        (Some(req), Some(id)) if req != id => f.push(format!("request {req} · response {id}")),
        (Some(req), _) => f.push(format!("request {req}")),
        (None, Some(id)) => f.push(format!("response {id}")),
        (None, None) => {}
    }
    if let Some(m) = str_at(&["model"]).or_else(|| str_at(&["modelVersion"])) {
        f.push(format!("model {m}"));
    }
    let tier = str_at(&["service_tier"])
        .or_else(|| str_at(&["usage", "service_tier"]))
        .or_else(|| str_at(&["usageMetadata", "serviceTier"]))
        .or_else(|| c.header("x-gemini-service-tier").map(str::to_string));
    if let Some(t) = tier {
        f.push(format!("tier {t}"));
    }
    if let Some(g) = str_at(&["usage", "inference_geo"]) {
        f.push(format!("geo {g}"));
    }
    if let Some(fp) = str_at(&["system_fingerprint"]) {
        f.push(format!("fingerprint {fp}"));
    }
    let secs = |ms: u64| format!("{:.1}s", ms as f64 / 1000.0);
    match (c.ms_headers, c.ms_first_event) {
        (Some(h), Some(e)) => f.push(format!("headers {} · first event {}", secs(h), secs(e))),
        (Some(h), None) => f.push(format!("headers {}", secs(h))),
        _ => {}
    }
    if let Some(p) = c.header("openai-processing-ms").and_then(|v| v.parse::<u64>().ok()) {
        f.push(format!("processing {}", secs(p)));
    }
    // Rate limits: remaining / limit per dimension.
    let mut limits = Vec::new();
    for (k, v) in &c.headers {
        let dim = k
            .strip_prefix("x-ratelimit-remaining-")
            .map(|d| (d.to_string(), format!("x-ratelimit-limit-{d}")))
            .or_else(|| {
                k.strip_prefix("anthropic-ratelimit-")
                    .and_then(|x| x.strip_suffix("-remaining"))
                    .map(|d| (d.to_string(), format!("anthropic-ratelimit-{d}-limit")))
            });
        if let Some((d, lim)) = dim
            && let Some(l) = c.header(&lim)
        {
            limits.push(format!("{v}/{l} {}", d.replace('-', " ")));
        }
    }
    if !limits.is_empty() {
        f.push(format!("rate limits {}", limits.join(", ")));
    }
    (!f.is_empty()).then(|| format!("  {}", f.join(" · ")))
}

async fn cmd_retry(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let Some(t) = target_turn(app, bot, ib).await? else { return Ok(()) };
    if generating(&t) {
        say(app, bot, ib, STILL_GENERATING).await;
        return Ok(());
    }
    if t.status != TurnStatus::Failed {
        say(app, bot, ib, "only failed answers can be retried; for another answer, send the question again").await;
        return Ok(());
    }
    if !retry_turn(app, t.id).await? {
        say(app, bot, ib, "this answer is already being retried").await;
    }
    Ok(())
}

/// The "🔁 Retry" button under a reply that finally failed (callback data
/// `retry`; `retry:<turn id>` on older buttons). Anyone allowed to use the bot in the chat may press it,
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
    // `retry`: the reply the button is under (buttons carry no turn id, which would
    // tell how much the bot is used elsewhere); `retry:<turn id>`: older buttons.
    let claimed = match data {
        "retry" => None,
        d => match d.strip_prefix("retry:").and_then(|s| s.parse::<i64>().ok()) {
            Some(id) => Some(id),
            None => {
                answer(None).await;
                return Ok(());
            }
        },
    };
    // The button must belong to a message of that reply, in this chat, sent by this bot.
    let Some(turn_id) = conv::turn_for_message(&app.db, &bot.name, chat_id, q.msg_id).await?.map(|(t, _)| t) else {
        answer(Some("This button does not belong to this message.")).await;
        return Ok(());
    };
    if claimed.is_some_and(|c| c != turn_id) {
        answer(Some("This button does not belong to this message.")).await;
        return Ok(());
    }
    // A retry is a paid request.
    match access::check(app, &bot.name, chat_id, q.user_id).await? {
        Access::Allowed => {}
        Access::Banned => {
            answer(Some(&format!("You are banned from using this bot in {}.", access::this_chat(chat_id)))).await;
            return Ok(());
        }
        Access::NotAllowed => {
            answer(None).await;
            return Ok(());
        }
    }
    let t = conv::get_turn(&app.db, turn_id).await?;
    if t.status != TurnStatus::Failed {
        answer(Some("This answer has already been retried.")).await;
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
    if generating(&t) {
        say(app, bot, ib, STILL_GENERATING).await;
        return Ok(());
    }
    if t.status == TurnStatus::Failed {
        say(app, bot, ib, "this answer failed; /retry it instead").await;
        return Ok(());
    }
    if t.status != TurnStatus::Done || !jobs::can_continue(app, t.id).await? {
        say(
            app,
            bot,
            ib,
            "/continue only works on a long answer whose note says to reply /continue; to get more of another \
             answer, reply to it (e.g. \"go on\")",
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
    // It must reply to an answer (says so otherwise).
    if target_turn(app, bot, ib).await?.is_none() {
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
        Some(s) => {
            (s.timestamp().as_millisecond(), format!("{label} (since {} {tz_name})", s.strftime("%Y-%m-%d %H:%M")))
        }
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
            // As shown in replies: `provider:model`, OpenRouter included.
            Dim::Model => (
                "COALESCE(l.provider, 'openrouter') || ':' || l.model".into(),
                "COALESCE(l.provider, 'openrouter'), l.model",
            ),
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
        (Some(chat), None, access::this_chat(chat).into())
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
                .map(|(k, n, i, o, c)| {
                    vec![k, n.to_string(), fmt_tokens_compact(i), fmt_tokens_compact(o), fmt_cost(c)]
                })
                .collect();
            blocks.push(table(&[d.header(), "API calls", "tokens in", "tokens out", "cost"], rows));
        }
    }
    send(app, bot, ib, "Usage stats", blocks).await;
    Ok(())
}

/// `/usage`: this chat's spend limit and your own limit in this chat.
async fn cmd_usage(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound) -> Result<()> {
    let user = ib.sender_id.unwrap_or(0);
    let mut lines = Vec::new();
    let here = access::this_chat(ib.chat_id);
    for (what, target) in
        [(here.to_string(), access::Target::Chat), (format!("you in {here}"), access::Target::Member(user))]
    {
        lines.push(match access::level(&app.db, &bot.name, ib.chat_id, target).await? {
            Some((lvl, r)) => format!("{what}: {}", access::balance_text(lvl, &r)),
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
/// `/quota`: spend limits. Anyone sees this chat's; admins change them (in a
/// group that group's, in their private chat any chat's).
async fn cmd_quota(app: &Arc<App>, bot: &Arc<BotConn>, ib: &Inbound, w: &[&str], by: i64, admin: bool) -> Result<()> {
    use access::Target;
    // Someone who is not an admin only ever sees the chat they are in.
    let group = ib.chat_id < 0 || !admin;
    let usage = if !admin {
        "usage: /quota [chat|members|<user_id>] (or /quota as a reply) shows this chat's spend limits; only admins change them"
    } else if group {
        "usage in a group: /quota [chat|members|<user_id> [<capacity_usd> <refill_usd_per_day> | off | reset [<balance_usd>]]]; as a reply: /quota [<capacity_usd> <refill_usd_per_day> | off | reset [<balance_usd>]] for that person"
    } else {
        "usage: /quota [<chat_id> [chat|members|<user_id> [<capacity_usd> <refill_usd_per_day> | off | reset [<balance_usd>]]]]"
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
    // Two numbers alone are a capacity and a refill (for the replied person);
    // a number and `off` are a user id and `off`.
    let target = match w.first().copied() {
        Some("chat") => Some(Target::Chat),
        Some("members") => Some(Target::Members),
        Some(x) if x.parse::<i64>().is_ok_and(|u| u > 0) && (w.len() != 2 || w[1] == "off" || w[1] == "reset") => {
            Some(Target::Member(x.parse().unwrap()))
        }
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
            let title = format!("Spend limits of {}", access::this_chat(chat));
            send(app, bot, ib, &title, quota_blocks(app, &bot.name, Some(chat)).await?).await;
            return Ok(());
        }
        None => match replied_sender(app, bot, ib).await {
            Some(u) if ib.chat_id < 0 => Target::Member(u),
            _ => {
                say(app, bot, ib, usage).await;
                return Ok(());
            }
        },
    };
    let here = if chat == ib.chat_id { access::this_chat(chat).to_string() } else { place_label(chat) };
    let who = match target {
        Target::Chat => here.clone(),
        Target::Members => format!("each member of {here}"),
        Target::Member(u) => format!("user {u} in {here}"),
    };
    if !admin && !w.is_empty() {
        say(app, bot, ib, &format!("⛔ only admins change spend limits; /quota shows {here}'s")).await;
        return Ok(());
    }
    // `reset [<balance_usd>]`: the balance back to the limit's capacity, or to an amount.
    if w.first() == Some(&"reset") {
        let amount = match w.get(1) {
            None => None,
            Some(x) => match x.parse::<f64>() {
                Ok(v) if v.is_finite() && w.len() == 2 => Some(v),
                _ => {
                    say(app, bot, ib, usage).await;
                    return Ok(());
                }
            },
        };
        let text = match access::reset(&app.db, &bot.name, chat, target, amount).await? {
            access::Reset::NoRule => format!("{who}: no limit, so there is no balance to reset"),
            access::Reset::AboveCapacity(r) => {
                format!("⚠️ {who}: a balance is at most the limit, {} USD", access::usd(r.capacity))
            }
            access::Reset::Balance(l, r) => format!("✅ {who}: {}", access::balance_text(l, &r)),
            access::Reset::Members(n, r) => match amount {
                None => {
                    format!("✅ {who}: every balance is back to the full {} USD ({n} reset)", access::usd(r.capacity))
                }
                Some(a) => format!(
                    "✅ {who}: balance {} USD for the {n} who have used the bot here (anyone else is at the full {} USD; \
                     members with a limit of their own keep theirs)",
                    access::usd(a),
                    access::usd(r.capacity)
                ),
            },
        };
        say(app, bot, ib, &text).await;
        return Ok(());
    }
    let rule = match (w.first(), w.get(1)) {
        (None, None) => {
            // Show it.
            let text = match (target, access::level(&app.db, &bot.name, chat, target).await?) {
                (Target::Members, _) => match access::rule_for(&app.db, &bot.name, chat, Target::Members).await? {
                    Some(r) => format!(
                        "{who}: a limit of {} USD, refilling {} USD/day",
                        access::usd(r.capacity),
                        access::usd(r.refill_per_day)
                    ),
                    None => format!("{who}: no limit"),
                },
                (_, Some((lvl, r))) => {
                    let default = match target {
                        Target::Member(_) if !access::has_own_rule(&app.db, &bot.name, chat, target).await? => {
                            " (the default for members)"
                        }
                        _ => "",
                    };
                    format!("{who}: {}{default}", access::balance_text(lvl, &r))
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
        Some(r) => format!(
            "✅ {who}: a limit of {} USD, refilling {} USD/day",
            access::usd(r.capacity),
            access::usd(r.refill_per_day)
        ),
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
            Target::Chat => (
                if q.chat_id < 0 { "whole group" } else { "whole chat" }.to_string(),
                access::level(&app.db, bot, q.chat_id, q.target).await?,
            ),
            Target::Members => ("each member".to_string(), None),
            Target::Member(u) => (format!("user {u}"), access::level(&app.db, bot, q.chat_id, q.target).await?),
        };
        let balance = balance.map(|(l, _)| access::usd(l)).unwrap_or("-".into());
        let mut row = vec![target, access::usd(q.rule.capacity), access::usd(q.rule.refill_per_day), balance];
        if only_chat.is_none() {
            row.insert(0, place_label(q.chat_id));
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Ok(vec![RBlock::Paragraph(RText::plain("No spend limits."))]);
    }
    let header: &[&str] = if only_chat.is_none() {
        &["chat", "applies to", "limit USD", "refill USD/day", "balance USD"]
    } else {
        &["applies to", "limit USD", "refill USD/day", "balance USD"]
    };
    Ok(vec![table(header, rows)])
}
