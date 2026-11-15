//! Access control and spend limits (token buckets in USD), as a tree:
//! bot → chat → person (owner's rule). Every bot is independent. Under a bot,
//! groups and private chats are equal "chats": each is allowed or not, and
//! everything else lives inside one chat — bans of people there, the chat's
//! spend limit, its members' limits. Nothing set or counted in one chat ever
//! affects another.
//!
//! Where a chat is not allowed, only /ping and an administrator's /allow are
//! answered. In an allowed chat,
//! queries (commands that only show information) are answered for everyone,
//! people banned there included, and paid requests for everyone not banned
//! there (banned people are told). Nobody is special: the administrators'
//! private chats are allowed once at the first start, and they can remove them
//! or ban themselves like anyone.
//! Administrators manage this with commands: in their private chat for every
//! chat of the bot, inside a group for that group only (see `commands`).

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::app::App;
use crate::store::{Db, now_ms};

/// What a person may do in a chat. Queries (information that costs nothing)
/// are always answered; this decides paid requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The chat is allowed and the person is not banned there.
    Allowed,
    /// The chat is allowed, but the person is banned there: told so.
    Banned,
    /// The chat is not allowed: only /ping (and an administrator's /allow) is answered.
    NotAllowed,
}

/// Allow the configured chats (groups and private chats) for every bot, once
/// per (bot, chat): later changes by commands win.
pub async fn seed(db: &Db, bots: &[String], chats: &[i64]) -> Result<()> {
    let (bots, places): (Vec<String>, Vec<i64>) = (bots.to_vec(), chats.to_vec());
    db.call(move |c| {
        let seeded: Option<String> = c
            .query_row("SELECT value FROM meta WHERE key = 'access_seeded'", [], |r| r.get(0))
            .optional()?;
        let mut done: Vec<String> = seeded.map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
        for bot in &bots {
            for &id in &places {
                let key = format!("{bot}:{id}");
                if done.contains(&key) {
                    continue;
                }
                c.execute(
                    "INSERT OR IGNORE INTO access_rules(bot, chat_id, user_id, status, note, set_at)
                     VALUES (?1, ?2, 0, 'allow', 'config', ?3)",
                    params![bot, id, now_ms()],
                )?;
                done.push(key);
            }
        }
        c.execute(
            "INSERT INTO meta(key, value) VALUES ('access_seeded', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![done.join(",")],
        )?;
        Ok(())
    })
    .await
}

/// Whether a chat (a group, or the private chat of a user) is allowed.
pub async fn place_allowed(db: &Db, bot: &str, chat_id: i64) -> Result<bool> {
    let bot = bot.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT 1 FROM access_rules WHERE bot = ?1 AND chat_id = ?2 AND user_id = 0 AND status = 'allow'",
            params![bot, chat_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
    .await
}

/// Allow a chat, or remove it from the allow list. `false` if nothing changed.
pub async fn set_place(db: &Db, bot: &str, chat_id: i64, allow: bool, by: i64) -> Result<bool> {
    let bot = bot.to_string();
    db.call(move |c| {
        let n = if allow {
            c.execute(
                "INSERT OR IGNORE INTO access_rules(bot, chat_id, user_id, status, set_by, set_at)
                 VALUES (?1, ?2, 0, 'allow', ?3, ?4)",
                params![bot, chat_id, by, now_ms()],
            )?
        } else {
            c.execute(
                "DELETE FROM access_rules WHERE bot = ?1 AND chat_id = ?2 AND user_id = 0",
                params![bot, chat_id],
            )?
        };
        Ok(n > 0)
    })
    .await
}

/// Ban a person in one chat, or lift that ban. `false` if nothing changed.
pub async fn set_ban(db: &Db, bot: &str, chat_id: i64, user_id: i64, ban: bool, by: i64) -> Result<bool> {
    let bot = bot.to_string();
    db.call(move |c| {
        let n = if ban {
            c.execute(
                "INSERT OR IGNORE INTO access_rules(bot, chat_id, user_id, status, set_by, set_at)
                 VALUES (?1, ?2, ?3, 'deny', ?4, ?5)",
                params![bot, chat_id, user_id, by, now_ms()],
            )?
        } else {
            c.execute(
                "DELETE FROM access_rules WHERE bot = ?1 AND chat_id = ?2 AND user_id = ?3 AND status = 'deny'",
                params![bot, chat_id, user_id],
            )?
        };
        Ok(n > 0)
    })
    .await
}

/// A basic group became a supergroup (a new chat id): it is the same group, so
/// it keeps everything it had there — being allowed, its bans, spend limits
/// and balances, and its prefixes — in one transaction.
pub async fn migrate_chat(db: &Db, bot: &str, from: i64, to: i64) -> Result<()> {
    let bot = bot.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        for sql in [
            "INSERT OR IGNORE INTO access_rules(bot, chat_id, user_id, status, note, set_by, set_at)
             SELECT bot, ?3, user_id, status, note, set_by, set_at FROM access_rules WHERE bot = ?1 AND chat_id = ?2",
            "INSERT OR IGNORE INTO quota_rules(bot, chat_id, target, user_id, capacity_usd, refill_usd_per_day, set_by, set_at)
             SELECT bot, ?3, target, user_id, capacity_usd, refill_usd_per_day, set_by, set_at FROM quota_rules
             WHERE bot = ?1 AND chat_id = ?2",
            "INSERT OR IGNORE INTO quota_buckets(bot, chat_id, target, user_id, level_usd, updated_at)
             SELECT bot, ?3, target, user_id, level_usd, updated_at FROM quota_buckets WHERE bot = ?1 AND chat_id = ?2",
            "INSERT OR IGNORE INTO chat_prefixes(bot, chat_id, name, spec, turns, created_by, created_at)
             SELECT bot, ?3, name, spec, turns, created_by, created_at FROM chat_prefixes WHERE bot = ?1 AND chat_id = ?2",
        ] {
            tx.execute(sql, params![bot, from, to])?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

/// Whether a person is banned in this chat.
pub async fn banned(db: &Db, bot: &str, chat_id: i64, user_id: i64) -> Result<bool> {
    let bot = bot.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT 1 FROM access_rules WHERE bot = ?1 AND user_id = ?2 AND status = 'deny' AND chat_id = ?3",
            params![bot, user_id, chat_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
    .await
}

/// One access rule with display names.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleRow {
    pub chat_id: i64,
    pub user_id: i64,
    pub status: String,
    pub place: Option<String>,
    pub user: Option<String>,
}

/// The rules of a bot: all of them (`None`), or only those of one chat.
pub async fn rules(db: &Db, bot: &str, only_chat: Option<i64>) -> Result<Vec<RuleRow>> {
    let bot = bot.to_string();
    db.call(move |c| {
        let mut s = c.prepare(
            "SELECT a.chat_id, a.user_id, a.status,
                    COALESCE(ch.title, NULLIF(TRIM(COALESCE(pu.first_name,'') || ' ' || COALESCE(pu.last_name,'') || COALESCE(' @' || pu.username, '')), '')),
                    NULLIF(TRIM(COALESCE(u.first_name,'') || ' ' || COALESCE(u.last_name,'') || COALESCE(' @' || u.username, '')), '')
             FROM access_rules a
             LEFT JOIN chats ch ON ch.chat_id = a.chat_id
             LEFT JOIN users pu ON a.chat_id > 0 AND pu.user_id = a.chat_id
             LEFT JOIN users u ON a.user_id <> 0 AND u.user_id = a.user_id
             WHERE a.bot = ?1 AND (?2 IS NULL OR a.chat_id = ?2)
             ORDER BY a.status, a.chat_id, a.user_id",
        )?;
        let v = s
            .query_map(params![bot, only_chat], |r| {
                Ok(RuleRow { chat_id: r.get(0)?, user_id: r.get(1)?, status: r.get(2)?, place: r.get(3)?, user: r.get(4)? })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
}

/// Whether `user_id` may make paid requests to `bot` in `chat_id`. Nobody is
/// special: administrators can remove their own private chat from the allow
/// list or ban themselves like anyone else.
pub async fn check(app: &App, bot: &str, chat_id: i64, user_id: i64) -> Result<Access> {
    let place = place_allowed(&app.db, bot, chat_id).await?;
    let banned = banned(&app.db, bot, chat_id, user_id).await?;
    Ok(decide(place, banned))
}

/// [`check`] given the rules: whether the chat is allowed, whether the person is banned there.
pub fn decide(chat_allowed: bool, banned: bool) -> Access {
    match (chat_allowed, banned) {
        (false, _) => Access::NotAllowed,
        (true, true) => Access::Banned,
        (true, false) => Access::Allowed,
    }
}

// ------------------------------------------------------------------ quotas

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub capacity: f64,
    pub refill_per_day: f64,
}

/// What a spend limit applies to, inside one chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The chat as a whole.
    Chat,
    /// Each member of the chat that has no limit of their own.
    Members,
    /// One member of the chat.
    Member(i64),
}

impl Target {
    fn key(self) -> (&'static str, i64) {
        match self {
            Target::Chat => ("chat", 0),
            Target::Members => ("member", 0),
            Target::Member(u) => ("member", u),
        }
    }
}

/// The rule of a target in a chat: its own, or for a member the chat's
/// default for members.
pub async fn rule_for(db: &Db, bot: &str, chat_id: i64, target: Target) -> Result<Option<Rule>> {
    let bot = bot.to_string();
    db.call(move |c| rule_in(c, &bot, chat_id, target)).await
}

fn rule_in(c: &Connection, bot: &str, chat_id: i64, target: Target) -> Result<Option<Rule>> {
    let (kind, user) = target.key();
    Ok(c.query_row(
        "SELECT capacity_usd, refill_usd_per_day FROM quota_rules
         WHERE bot = ?1 AND chat_id = ?2 AND target = ?3 AND user_id IN (?4, 0) ORDER BY user_id = ?4 DESC LIMIT 1",
        params![bot, chat_id, kind, user],
        |r| Ok(Rule { capacity: r.get(0)?, refill_per_day: r.get(1)? }),
    )
    .optional()?)
}

/// Whether a member has a limit of their own (not just the chat's default for members).
pub async fn has_own_rule(db: &Db, bot: &str, chat_id: i64, target: Target) -> Result<bool> {
    let bot = bot.to_string();
    let (kind, user) = target.key();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT 1 FROM quota_rules WHERE bot = ?1 AND chat_id = ?2 AND target = ?3 AND user_id = ?4",
            params![bot, chat_id, kind, user],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
    })
    .await
}

pub async fn set_rule(db: &Db, bot: &str, chat_id: i64, target: Target, rule: Option<Rule>, by: i64) -> Result<()> {
    let bot = bot.to_string();
    let (kind, user) = target.key();
    db.call(move |c| {
        match rule {
            Some(r) => c.execute(
                "INSERT INTO quota_rules(bot, chat_id, target, user_id, capacity_usd, refill_usd_per_day, set_by, set_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(bot, chat_id, target, user_id) DO UPDATE SET capacity_usd = excluded.capacity_usd,
                   refill_usd_per_day = excluded.refill_usd_per_day, set_by = excluded.set_by, set_at = excluded.set_at",
                params![bot, chat_id, kind, user, r.capacity, r.refill_per_day, by, now_ms()],
            )?,
            None => c.execute(
                "DELETE FROM quota_rules WHERE bot = ?1 AND chat_id = ?2 AND target = ?3 AND user_id = ?4",
                params![bot, chat_id, kind, user],
            )?,
        };
        Ok(())
    })
    .await
}

/// One spend-limit rule as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaRow {
    pub chat_id: i64,
    pub target: Target,
    pub rule: Rule,
}

/// The spend-limit rules of a bot: all (`None`) or one chat's.
pub async fn quota_rules(db: &Db, bot: &str, only_chat: Option<i64>) -> Result<Vec<QuotaRow>> {
    let bot = bot.to_string();
    db.call(move |c| {
        let mut s = c.prepare(
            "SELECT chat_id, target, user_id, capacity_usd, refill_usd_per_day FROM quota_rules
             WHERE bot = ?1 AND (?2 IS NULL OR chat_id = ?2) ORDER BY chat_id, target, user_id",
        )?;
        let v = s
            .query_map(params![bot, only_chat], |r| {
                let (kind, user): (String, i64) = (r.get(1)?, r.get(2)?);
                let target = match (kind.as_str(), user) {
                    ("chat", _) => Target::Chat,
                    (_, 0) => Target::Members,
                    (_, u) => Target::Member(u),
                };
                Ok(QuotaRow {
                    chat_id: r.get(0)?,
                    target,
                    rule: Rule { capacity: r.get(3)?, refill_per_day: r.get(4)? },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
}

/// Current bucket level (USD) after refill of the chat or of one member in
/// it; `None` if no rule applies. (`Members` has no bucket of its own.)
pub async fn level(db: &Db, bot: &str, chat_id: i64, target: Target) -> Result<Option<(f64, Rule)>> {
    let bot = bot.to_string();
    db.call(move |c| level_in(c, &bot, chat_id, target, now_ms())).await
}

fn level_in(c: &Connection, bot: &str, chat_id: i64, target: Target, now: i64) -> Result<Option<(f64, Rule)>> {
    let Some(rule) = rule_in(c, bot, chat_id, target)? else { return Ok(None) };
    let (kind, user) = target.key();
    let stored: Option<(f64, i64)> = c
        .query_row(
            "SELECT level_usd, updated_at FROM quota_buckets WHERE bot = ?1 AND chat_id = ?2 AND target = ?3 AND user_id = ?4",
            params![bot, chat_id, kind, user],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let lvl = match stored {
        None => rule.capacity,
        Some((l, t)) => {
            let days = (now - t).max(0) as f64 / 86_400_000.0;
            (l + days * rule.refill_per_day).min(rule.capacity)
        }
    };
    Ok(Some((lvl, rule)))
}

/// Deduct `cost` spent by `user_id` in a chat from the chat's bucket and the
/// member's (each only if a rule applies; may go negative: the next request
/// is then blocked until refilled). One transaction, so that concurrent
/// charges never overwrite each other. (The ledger uses [`charge_in`].)
#[cfg(test)]
pub async fn charge(db: &Db, bot: &str, chat_id: i64, user_id: i64, cost: f64) -> Result<()> {
    let bot = bot.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        charge_in(&tx, &bot, chat_id, user_id, cost)?;
        tx.commit()?;
        Ok(())
    })
    .await
}

/// [`charge`] inside the caller's transaction (the ledger charges together
/// with the row it records). Reading the level and writing it back happen
/// under the same database lock, so no other charge can come in between.
pub fn charge_in(c: &Connection, bot: &str, chat_id: i64, user_id: i64, cost: f64) -> Result<()> {
    let now = now_ms();
    for target in [Target::Chat, Target::Member(user_id)] {
        let Some((lvl, _)) = level_in(c, bot, chat_id, target, now)? else { continue };
        let (kind, user) = target.key();
        c.execute(SET_BUCKET, params![bot, chat_id, kind, user, lvl - cost, now])?;
    }
    Ok(())
}

/// A bucket's level from now on (it refills from there).
const SET_BUCKET: &str = "INSERT INTO quota_buckets(bot, chat_id, target, user_id, level_usd, updated_at)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
     ON CONFLICT(bot, chat_id, target, user_id) DO UPDATE SET level_usd = excluded.level_usd, updated_at = excluded.updated_at";

/// Every balance of a chat back to full: the chat's and each member's (the
/// rules stay). Returns how many limits the chat has (0: nothing to reset).
pub async fn reset_all(db: &Db, bot: &str, chat_id: i64) -> Result<usize> {
    let bot = bot.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        let rules: i64 = tx.query_row(
            "SELECT COUNT(*) FROM quota_rules WHERE bot = ?1 AND chat_id = ?2",
            params![bot, chat_id],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM quota_buckets WHERE bot = ?1 AND chat_id = ?2", params![bot, chat_id])?;
        tx.commit()?;
        Ok(rules as usize)
    })
    .await
}

/// What resetting a balance did.
#[derive(Debug, Clone, PartialEq)]
pub enum Reset {
    /// No limit applies there: nothing to reset.
    NoRule,
    /// The amount asked for is above the limit's capacity.
    AboveCapacity(Rule),
    /// The chat's or one member's balance is now this.
    Balance(f64, Rule),
    /// Each member on the chat's default limit: how many balances were reset.
    Members(usize, Rule),
}

/// Reset a balance to its capacity (`amount` `None`: the bucket goes, which
/// reads as full) or to `amount` USD (from which it refills; at most the
/// capacity, negative is a debt). `Members` resets every member on the chat's
/// default limit (not those with a limit of their own): back to full, or to
/// `amount` for everyone the bot has seen use it there (anyone else is full
/// anyway). One transaction.
pub async fn reset(db: &Db, bot: &str, chat_id: i64, target: Target, amount: Option<f64>) -> Result<Reset> {
    let bot = bot.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        let Some(rule) = rule_in(&tx, &bot, chat_id, target)? else { return Ok(Reset::NoRule) };
        if amount.is_some_and(|a| a > rule.capacity) {
            return Ok(Reset::AboveCapacity(rule));
        }
        let now = now_ms();
        let out = match target {
            Target::Chat | Target::Member(_) => {
                let (kind, user) = target.key();
                match amount {
                    None => tx.execute(
                        "DELETE FROM quota_buckets WHERE bot = ?1 AND chat_id = ?2 AND target = ?3 AND user_id = ?4",
                        params![bot, chat_id, kind, user],
                    )?,
                    Some(a) => tx.execute(SET_BUCKET, params![bot, chat_id, kind, user, a, now])?,
                };
                Reset::Balance(amount.unwrap_or(rule.capacity), rule)
            }
            Target::Members => {
                // Members with a limit of their own are not on the default.
                const OWN: &str = "SELECT user_id FROM quota_rules
                     WHERE bot = ?1 AND chat_id = ?2 AND target = 'member' AND user_id <> 0";
                let n = match amount {
                    None => tx.execute(
                        &format!(
                            "DELETE FROM quota_buckets WHERE bot = ?1 AND chat_id = ?2 AND target = 'member'
                             AND user_id NOT IN ({OWN})"
                        ),
                        params![bot, chat_id],
                    )?,
                    Some(a) => {
                        let users: Vec<i64> = {
                            let mut s = tx.prepare(&format!(
                                "SELECT user_id FROM usage_ledger WHERE bot = ?1 AND chat_id = ?2 AND user_id > 0
                                 UNION SELECT user_id FROM quota_buckets WHERE bot = ?1 AND chat_id = ?2 AND target = 'member'
                                 EXCEPT {OWN}"
                            ))?;
                            s.query_map(params![bot, chat_id], |r| r.get(0))?.collect::<Result<_, _>>()?
                        };
                        for u in &users {
                            tx.execute(SET_BUCKET, params![bot, chat_id, "member", u, a, now])?;
                        }
                        users.len()
                    }
                };
                Reset::Members(n, rule)
            }
        };
        tx.commit()?;
        Ok(out)
    })
    .await
}

/// A spend-limit amount: two decimals, a minus sign when in debt (`-0.50`).
pub fn usd(v: f64) -> String {
    format!("{v:.2}")
}

/// A spend-limit balance in words: `balance 1.23 of 5.00 USD, refills 1.00 USD/day`.
pub fn balance_text(level: f64, rule: &Rule) -> String {
    let refill = if rule.refill_per_day > 0.0 {
        format!("refills {} USD/day", usd(rule.refill_per_day))
    } else {
        "does not refill".to_string()
    };
    format!("balance {} of {} USD, {refill}", usd(level), usd(rule.capacity))
}

/// What a chat is called in messages: a group is "this group", a private
/// chat "this chat".
pub fn this_chat(chat_id: i64) -> &'static str {
    if chat_id < 0 { "this group" } else { "this chat" }
}

/// `Err(message)` if this chat's bucket or the member's bucket in it is
/// exhausted (for everyone, administrators included).
pub async fn check_quota(app: &App, bot: &str, chat_id: i64, user_id: i64) -> Result<Result<(), String>> {
    let here = this_chat(chat_id);
    for (what, target) in [
        (format!("{here}'s spend limit"), Target::Chat),
        (format!("your spend limit in {here}"), Target::Member(user_id)),
    ] {
        if let Some((lvl, rule)) = level(&app.db, bot, chat_id, target).await?
            && lvl <= 0.0
        {
            // Usable again once the bucket is back above zero.
            let wait_h = if rule.refill_per_day > 0.0 { -lvl / rule.refill_per_day * 24.0 } else { f64::INFINITY };
            return Ok(Err(format!(
                "{what} is used up ({}); {}",
                balance_text(lvl, &rule),
                if wait_h.is_finite() {
                    format!("usable again in about {:.1} h", wait_h.max(0.1))
                } else {
                    "it does not refill: ask an admin".to_string()
                }
            )));
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn limits_live_in_one_chat() {
        let db = Db::open_in_memory().unwrap();
        let (g, h, u) = (-100, -200, 7);
        assert!(level(&db, "a", g, Target::Member(u)).await.unwrap().is_none());
        // Each member of g by default.
        set_rule(&db, "a", g, Target::Members, Some(Rule { capacity: 1.0, refill_per_day: 1.0 }), 9).await.unwrap();
        let (l, _) = level(&db, "a", g, Target::Member(u)).await.unwrap().unwrap();
        assert_eq!(l, 1.0);
        charge(&db, "a", g, u, 1.5).await.unwrap();
        let (l, _) = level(&db, "a", g, Target::Member(u)).await.unwrap().unwrap();
        assert!(l < 0.0 && l > -0.51);
        // Nothing of it in another chat or another bot.
        assert!(level(&db, "a", h, Target::Member(u)).await.unwrap().is_none());
        assert!(level(&db, "b", g, Target::Member(u)).await.unwrap().is_none());
        // A member's own rule beats the default; the chat's own limit is separate.
        set_rule(&db, "a", g, Target::Member(u), Some(Rule { capacity: 5.0, refill_per_day: 0.0 }), 9).await.unwrap();
        let (_, r) = level(&db, "a", g, Target::Member(u)).await.unwrap().unwrap();
        assert_eq!(r.capacity, 5.0);
        assert!(has_own_rule(&db, "a", g, Target::Member(u)).await.unwrap());
        assert!(level(&db, "a", g, Target::Chat).await.unwrap().is_none());
        set_rule(&db, "a", g, Target::Chat, Some(Rule { capacity: 2.0, refill_per_day: 2.0 }), 9).await.unwrap();
        charge(&db, "a", g, u, 0.5).await.unwrap();
        let (l, _) = level(&db, "a", g, Target::Chat).await.unwrap().unwrap();
        assert!((l - 1.5).abs() < 0.01);
        assert_eq!(quota_rules(&db, "a", Some(g)).await.unwrap().len(), 3);
        assert_eq!(quota_rules(&db, "a", Some(h)).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn concurrent_charges_all_count() {
        let db = Db::open_in_memory().unwrap();
        let (g, u) = (-100, 7);
        set_rule(&db, "a", g, Target::Members, Some(Rule { capacity: 100.0, refill_per_day: 0.0 }), 9).await.unwrap();
        set_rule(&db, "a", g, Target::Chat, Some(Rule { capacity: 100.0, refill_per_day: 0.0 }), 9).await.unwrap();
        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let db = db.clone();
                tokio::spawn(async move { charge(&db, "a", g, u, 1.0).await.unwrap() })
            })
            .collect();
        for t in tasks {
            t.await.unwrap();
        }
        for target in [Target::Chat, Target::Member(u)] {
            let (l, _) = level(&db, "a", g, target).await.unwrap().unwrap();
            assert!((l - 50.0).abs() < 1e-9, "{target:?}: {l}");
        }
    }

    #[tokio::test]
    async fn rules_are_per_bot_and_chat() {
        let db = Db::open_in_memory().unwrap();
        let (g, h, u) = (-100, -200, 7);
        assert!(set_place(&db, "a", g, true, 1).await.unwrap());
        assert!(!set_place(&db, "a", g, true, 1).await.unwrap());
        assert!(place_allowed(&db, "a", g).await.unwrap());
        assert!(!place_allowed(&db, "b", g).await.unwrap());
        assert!(!place_allowed(&db, "a", h).await.unwrap());
        // A ban in one chat applies there only.
        set_ban(&db, "a", g, u, true, 1).await.unwrap();
        assert!(banned(&db, "a", g, u).await.unwrap());
        assert!(!banned(&db, "a", h, u).await.unwrap());
        assert!(!banned(&db, "a", u, u).await.unwrap());
        assert!(!banned(&db, "b", g, u).await.unwrap());
        // A chat sees only its own rules.
        let only_g = rules(&db, "a", Some(g)).await.unwrap();
        assert_eq!(only_g.iter().map(|r| (r.chat_id, r.user_id)).collect::<Vec<_>>(), vec![(g, 0), (g, u)]);
        assert_eq!(rules(&db, "a", Some(h)).await.unwrap(), vec![]);
        assert!(set_ban(&db, "a", g, u, false, 1).await.unwrap());
        assert!(set_place(&db, "a", g, false, 1).await.unwrap());
        assert_eq!(rules(&db, "a", Some(g)).await.unwrap(), vec![]);
    }

    #[tokio::test]
    async fn balances_reset_to_full_or_an_amount() {
        let db = Db::open_in_memory().unwrap();
        let (g, u, v, own) = (-100, 7, 8, 9);
        let bal = |t| {
            let db = db.clone();
            async move { level(&db, "a", g, t).await.unwrap().unwrap().0 }
        };
        // No limit: nothing to reset.
        assert_eq!(reset(&db, "a", g, Target::Chat, None).await.unwrap(), Reset::NoRule);
        let r = Rule { capacity: 5.0, refill_per_day: 0.0 };
        set_rule(&db, "a", g, Target::Chat, Some(r), 1).await.unwrap();
        set_rule(&db, "a", g, Target::Members, Some(Rule { capacity: 1.0, refill_per_day: 0.0 }), 1).await.unwrap();
        set_rule(&db, "a", g, Target::Member(own), Some(Rule { capacity: 3.0, refill_per_day: 0.0 }), 1).await.unwrap();
        for who in [u, v, own] {
            charge(&db, "a", g, who, 0.5).await.unwrap();
        }
        // The chat: to full, to an amount, never above the capacity.
        assert_eq!(reset(&db, "a", g, Target::Chat, None).await.unwrap(), Reset::Balance(5.0, r));
        assert_eq!(bal(Target::Chat).await, 5.0);
        assert_eq!(reset(&db, "a", g, Target::Chat, Some(-1.0)).await.unwrap(), Reset::Balance(-1.0, r));
        assert_eq!(bal(Target::Chat).await, -1.0);
        assert_eq!(reset(&db, "a", g, Target::Chat, Some(6.0)).await.unwrap(), Reset::AboveCapacity(r));
        assert_eq!(bal(Target::Chat).await, -1.0);
        // One member on the default limit, and one with their own.
        reset(&db, "a", g, Target::Member(u), Some(0.2)).await.unwrap();
        assert!((bal(Target::Member(u)).await - 0.2).abs() < 1e-9);
        reset(&db, "a", g, Target::Member(own), None).await.unwrap();
        assert_eq!(bal(Target::Member(own)).await, 3.0);
        // Every member on the default: v back to full, u too; own keeps theirs.
        charge(&db, "a", g, own, 1.0).await.unwrap();
        assert!(matches!(reset(&db, "a", g, Target::Members, None).await.unwrap(), Reset::Members(2, _)));
        assert_eq!((bal(Target::Member(u)).await, bal(Target::Member(v)).await), (1.0, 1.0));
        assert_eq!(bal(Target::Member(own)).await, 2.0);
        // To an amount: everyone seen using the bot here who is on the default.
        db.call(|c| {
            c.execute(
                "INSERT INTO usage_ledger(ts, bot, chat_id, user_id, provider, model) VALUES (1, 'a', -100, 7, 'p', 'm'),
                 (1, 'a', -100, 9, 'p', 'm'), (1, 'a', -200, 8, 'p', 'm')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(matches!(reset(&db, "a", g, Target::Members, Some(0.3)).await.unwrap(), Reset::Members(1, _)));
        assert!((bal(Target::Member(u)).await - 0.3).abs() < 1e-9);
        assert_eq!((bal(Target::Member(v)).await, bal(Target::Member(own)).await), (1.0, 2.0));
    }

    #[tokio::test]
    async fn everything_resets_at_once() {
        let db = Db::open_in_memory().unwrap();
        let (g, h, u, own) = (-100, -200, 7, 9);
        assert_eq!(reset_all(&db, "a", g).await.unwrap(), 0);
        set_rule(&db, "a", g, Target::Chat, Some(Rule { capacity: 5.0, refill_per_day: 0.0 }), 1).await.unwrap();
        set_rule(&db, "a", g, Target::Members, Some(Rule { capacity: 1.0, refill_per_day: 0.0 }), 1).await.unwrap();
        set_rule(&db, "a", g, Target::Member(own), Some(Rule { capacity: 3.0, refill_per_day: 0.0 }), 1).await.unwrap();
        set_rule(&db, "a", h, Target::Chat, Some(Rule { capacity: 5.0, refill_per_day: 0.0 }), 1).await.unwrap();
        for who in [u, own] {
            charge(&db, "a", g, who, 0.5).await.unwrap();
        }
        charge(&db, "a", h, u, 0.5).await.unwrap();
        assert_eq!(reset_all(&db, "a", g).await.unwrap(), 3);
        for (t, full) in [(Target::Chat, 5.0), (Target::Member(u), 1.0), (Target::Member(own), 3.0)] {
            assert_eq!(level(&db, "a", g, t).await.unwrap().unwrap().0, full);
        }
        // Another chat keeps its balance.
        assert_eq!(level(&db, "a", h, Target::Chat).await.unwrap().unwrap().0, 4.5);
    }

    #[tokio::test]
    async fn a_migrated_group_keeps_its_rules() {
        let db = Db::open_in_memory().unwrap();
        let (old, new, u) = (-123, -1_000_000_000_123, 7);
        set_place(&db, "a", old, true, 1).await.unwrap();
        set_ban(&db, "a", old, u, true, 1).await.unwrap();
        set_rule(&db, "a", old, Target::Members, Some(Rule { capacity: 1.0, refill_per_day: 0.0 }), 9).await.unwrap();
        charge(&db, "a", old, u, 0.25).await.unwrap();
        migrate_chat(&db, "a", old, new).await.unwrap();
        assert!(place_allowed(&db, "a", new).await.unwrap());
        assert!(banned(&db, "a", new, u).await.unwrap());
        let (l, _) = level(&db, "a", new, Target::Member(u)).await.unwrap().unwrap();
        assert!((l - 0.75).abs() < 1e-9);
        // Another bot's rules stay its own.
        assert!(!place_allowed(&db, "b", new).await.unwrap());
    }

    #[test]
    fn paid_requests_need_an_allowed_chat_and_no_ban() {
        assert_eq!(decide(true, false), Access::Allowed);
        assert_eq!(decide(true, true), Access::Banned);
        assert_eq!(decide(false, false), Access::NotAllowed);
        assert_eq!(decide(false, true), Access::NotAllowed);
    }
}
