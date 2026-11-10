//! Access control (allow/deny lists) and spend limits (token buckets in USD).
//!
//! Roles are deliberately simple: administrators (from config), allowed
//! chats/users, and everyone else. Denied users are ignored everywhere.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use crate::app::App;
use crate::store::{Db, now_ms};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Allowed,
    /// Tell the user why (private chats).
    Denied(String),
    /// Ignore silently (groups that are not allowed, banned users).
    Silent,
}

pub async fn seed(db: &Db, chats: &[i64], users: &[i64]) -> Result<()> {
    let (chats, users) = (chats.to_vec(), users.to_vec());
    db.call(move |c| {
        for id in chats {
            c.execute(
                "INSERT OR IGNORE INTO acl(scope, id, status, note, set_at) VALUES ('chat', ?1, 'allow', 'config', ?2)",
                params![id, now_ms()],
            )?;
        }
        for id in users {
            c.execute(
                "INSERT OR IGNORE INTO acl(scope, id, status, note, set_at) VALUES ('user', ?1, 'allow', 'config', ?2)",
                params![id, now_ms()],
            )?;
        }
        Ok(())
    })
    .await
}

pub async fn acl_get(db: &Db, scope: &'static str, id: i64) -> Result<Option<String>> {
    db.call(move |c| {
        Ok(c.query_row("SELECT status FROM acl WHERE scope = ?1 AND id = ?2", params![scope, id], |r| r.get(0))
            .optional()?)
    })
    .await
}

pub async fn acl_set(
    db: &Db,
    scope: &'static str,
    id: i64,
    status: Option<&'static str>,
    by: i64,
    note: Option<String>,
) -> Result<()> {
    db.call(move |c| {
        match status {
            Some(s) => c.execute(
                "INSERT INTO acl(scope, id, status, note, set_by, set_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(scope, id) DO UPDATE SET status = excluded.status, note = excluded.note, set_by = excluded.set_by, set_at = excluded.set_at",
                params![scope, id, s, note, by, now_ms()],
            )?,
            None => c.execute("DELETE FROM acl WHERE scope = ?1 AND id = ?2", params![scope, id])?,
        };
        Ok(())
    })
    .await
}

pub async fn acl_list(db: &Db) -> Result<Vec<(String, i64, String, Option<String>)>> {
    db.call(|c| {
        let mut s = c.prepare(
            "SELECT a.scope, a.id, a.status, COALESCE(ch.title, TRIM(COALESCE(u.first_name,'') || ' ' || COALESCE(u.last_name,'') || COALESCE(' @' || u.username, '')))
             FROM acl a LEFT JOIN chats ch ON a.scope = 'chat' AND ch.chat_id = a.id
             LEFT JOIN users u ON a.scope = 'user' AND u.user_id = a.id ORDER BY a.scope, a.status, a.id",
        )?;
        let v = s
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    })
    .await
}

pub async fn check(app: &App, chat_id: i64, user_id: i64) -> Result<Access> {
    if app.is_admin(user_id) {
        return Ok(Access::Allowed);
    }
    if acl_get(&app.db, "user", user_id).await?.as_deref() == Some("deny") {
        return Ok(Access::Silent);
    }
    if chat_id > 0 {
        return Ok(match acl_get(&app.db, "user", user_id).await?.as_deref() {
            Some("allow") => Access::Allowed,
            _ => Access::Denied(format!(
                "You are not allowed to use this bot in private chat. Ask an administrator to run /allow_user {user_id}"
            )),
        });
    }
    Ok(match acl_get(&app.db, "chat", chat_id).await?.as_deref() {
        Some("allow") => Access::Allowed,
        _ => Access::Silent,
    })
}

// ------------------------------------------------------------------ quotas

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub capacity: f64,
    pub refill_per_day: f64,
}

pub async fn rule_for(db: &Db, scope: &'static str, id: i64) -> Result<Option<Rule>> {
    db.call(move |c| {
        let r = c
            .query_row(
                "SELECT capacity_usd, refill_usd_per_day FROM quota_rules WHERE scope = ?1 AND id IN (?2, 0) ORDER BY id DESC LIMIT 1",
                params![scope, id],
                |r| Ok(Rule { capacity: r.get(0)?, refill_per_day: r.get(1)? }),
            )
            .optional()?;
        Ok(r)
    })
    .await
}

pub async fn set_rule(db: &Db, scope: &'static str, id: i64, rule: Option<Rule>, by: i64) -> Result<()> {
    db.call(move |c| {
        match rule {
            Some(r) => c.execute(
                "INSERT INTO quota_rules(scope, id, capacity_usd, refill_usd_per_day, set_by, set_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(scope, id) DO UPDATE SET capacity_usd = excluded.capacity_usd, refill_usd_per_day = excluded.refill_usd_per_day, set_by = excluded.set_by, set_at = excluded.set_at",
                params![scope, id, r.capacity, r.refill_per_day, by, now_ms()],
            )?,
            None => c.execute("DELETE FROM quota_rules WHERE scope = ?1 AND id = ?2", params![scope, id])?,
        };
        Ok(())
    })
    .await
}

/// Current bucket level (USD) after refill; `None` if no rule applies.
pub async fn level(db: &Db, scope: &'static str, id: i64) -> Result<Option<(f64, Rule)>> {
    let Some(rule) = rule_for(db, scope, id).await? else { return Ok(None) };
    let stored: Option<(f64, i64)> = db
        .call(move |c| {
            Ok(c.query_row(
                "SELECT level_usd, updated_at FROM quota_buckets WHERE scope = ?1 AND id = ?2",
                params![scope, id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await?;
    let now = now_ms();
    let lvl = match stored {
        None => rule.capacity,
        Some((l, t)) => {
            let days = (now - t).max(0) as f64 / 86_400_000.0;
            (l + days * rule.refill_per_day).min(rule.capacity)
        }
    };
    Ok(Some((lvl, rule)))
}

/// Deduct `cost` from the bucket (may go negative: the next request is then blocked until refilled).
pub async fn charge(db: &Db, scope: &'static str, id: i64, cost: f64) -> Result<()> {
    let Some((lvl, _)) = level(db, scope, id).await? else { return Ok(()) };
    let new = lvl - cost;
    db.call(move |c| {
        c.execute(
            "INSERT INTO quota_buckets(scope, id, level_usd, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope, id) DO UPDATE SET level_usd = excluded.level_usd, updated_at = excluded.updated_at",
            params![scope, id, new, now_ms()],
        )?;
        Ok(())
    })
    .await
}

/// `Err(message)` if the user's or the chat's bucket is exhausted.
pub async fn check_quota(app: &App, chat_id: i64, user_id: i64) -> Result<Result<(), String>> {
    if app.is_admin(user_id) {
        return Ok(Ok(()));
    }
    for (scope, id) in [("user", user_id), ("chat", chat_id)] {
        if scope == "chat" && chat_id > 0 {
            continue;
        }
        if let Some((lvl, rule)) = level(&app.db, scope, id).await?
            && lvl <= 0.0
        {
            let wait_h =
                if rule.refill_per_day > 0.0 { (-lvl + 0.0001) / rule.refill_per_day * 24.0 } else { f64::INFINITY };
            return Ok(Err(format!(
                "{scope} spend limit reached (bucket {lvl:.4} USD, capacity {:.2} USD, refill {:.2} USD/day){}",
                rule.capacity,
                rule.refill_per_day,
                if wait_h.is_finite() { format!("; available again in ~{wait_h:.1} h") } else { String::new() }
            )));
        }
    }
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bucket_refills_and_blocks() {
        let db = Db::open_in_memory().unwrap();
        assert!(level(&db, "user", 1).await.unwrap().is_none());
        set_rule(&db, "user", 0, Some(Rule { capacity: 1.0, refill_per_day: 1.0 }), 9).await.unwrap();
        let (l, _) = level(&db, "user", 1).await.unwrap().unwrap();
        assert_eq!(l, 1.0);
        charge(&db, "user", 1, 1.5).await.unwrap();
        let (l, _) = level(&db, "user", 1).await.unwrap().unwrap();
        assert!(l < 0.0 && l > -0.51);
        // Specific rule overrides the default.
        set_rule(&db, "user", 1, Some(Rule { capacity: 5.0, refill_per_day: 0.0 }), 9).await.unwrap();
        let (_, r) = level(&db, "user", 1).await.unwrap().unwrap();
        assert_eq!(r.capacity, 5.0);
    }
}
