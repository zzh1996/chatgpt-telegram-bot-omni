//! Chat prefixes: custom chatbots that members define for one chat.
//!
//! A chat prefix is a shorthand for a model with its options (all resolved when
//! it is created, so it keeps pointing at the same model) plus a system prompt
//! and a conversation history that go before every new message. Using it is
//! exactly like typing that model with those options and that history: the
//! reply, billing and stats are the model's, and the history is copied into the
//! conversation, so replies keep working after the prefix is deleted.
//!
//! It lives in one chat (tree bot → chat → person): anyone allowed there may use
//! it; its creator or an administrator may delete it.

use std::sync::Arc;

use anyhow::Result;
use omni_core::content::{Origin, Part, Role};
use omni_core::{Turn, trigger};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::conv::TurnRow;
use crate::store::{Db, now_ms};

/// Shortest and longest prefix name.
pub const NAME_LEN: std::ops::RangeInclusive<usize> = 3..=16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatPrefix {
    /// The model with all its options, resolved at creation
    /// (`openrouter:anthropic/claude-opus-5.5+s`).
    pub spec: String,
    /// System prompt and history sent before each new message.
    pub turns: Arc<Vec<Turn>>,
    pub created_by: i64,
    pub created_at: i64,
}

/// A valid prefix name, lowercased: 3–16 ASCII letters and digits.
pub fn check_name(name: &str) -> Result<String, String> {
    if !NAME_LEN.contains(&name.chars().count()) || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("a prefix name has {}–{} letters (a–z) and digits only", NAME_LEN.start(), NAME_LEN.end()));
    }
    Ok(name.to_ascii_lowercase())
}

/// The system prompt and history written with marker lines (`::system`,
/// `::user`, `::assistant`), checked by [`check_turns`].
pub fn turns_from_text(text: &str) -> Result<Vec<Turn>, String> {
    let segments = trigger::parse_history(text)
        .ok_or("write the system prompt and history with marker lines: ::system, ::user, ::assistant")?;
    let turns: Vec<Turn> = segments
        .into_iter()
        .map(|s| Turn { role: s.role, origin: Origin::Synthetic, parts: vec![Part::text(s.text)], native: None })
        .collect();
    check_turns(&turns)?;
    Ok(turns)
}

/// A conversation (root to the replied answer) as a prefix's turns: everything
/// as it was, marked as history.
pub fn turns_from_chain(chain: &[TurnRow]) -> Result<Vec<Turn>, String> {
    let turns: Vec<Turn> = chain.iter().map(|r| Turn { origin: Origin::Synthetic, ..r.turn.clone() }).collect();
    check_turns(&turns)?;
    Ok(turns)
}

/// A prefix's turns: at most one system prompt, first; no empty turn; and a
/// history that ends with the model's answer, so that the new message follows
/// as the user's turn.
pub fn check_turns(turns: &[Turn]) -> Result<(), String> {
    if turns.is_empty() {
        return Err("a prefix needs a system prompt or a history".into());
    }
    if turns.iter().skip(1).any(|t| t.role == Role::System) {
        return Err("only one system prompt, at the start".into());
    }
    if turns.iter().any(|t| t.parts.iter().all(|p| matches!(p, Part::Text { text, .. } if text.trim().is_empty()))) {
        return Err("a turn of the system prompt or history is empty".into());
    }
    match turns.last().map(|t| t.role) {
        Some(Role::Assistant | Role::System) => Ok(()),
        _ => Err("the history must end with the model's answer (::assistant), the new message comes after it".into()),
    }
}

pub async fn load_all(db: &Db) -> Result<Vec<(String, i64, String, ChatPrefix)>> {
    db.call(|c| {
        let mut s = c.prepare("SELECT bot, chat_id, name, spec, turns, created_by, created_at FROM chat_prefixes")?;
        let rows = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        for (bot, chat, name, spec, turns, created_by, created_at) in rows {
            let turns: Vec<Turn> = serde_json::from_str(&turns)?;
            out.push((bot, chat, name, ChatPrefix { spec, turns: Arc::new(turns), created_by, created_at }));
        }
        Ok(out)
    })
    .await
}

/// Prefixes one chat may have (they are all kept in memory): far more than any
/// chat uses, few enough that nobody can fill the bot's memory with them.
pub const MAX_PER_CHAT: i64 = 200;

/// What storing a new prefix did.
#[derive(Debug, PartialEq, Eq)]
pub enum Inserted {
    Added,
    /// The chat already has one by that name.
    Taken,
    /// The chat has [`MAX_PER_CHAT`] already.
    Full,
}

/// Store a new prefix (the count and the insert in one call: two creations at
/// once cannot pass the limit together).
pub async fn insert(db: &Db, bot: &str, chat_id: i64, name: &str, p: &ChatPrefix) -> Result<Inserted> {
    let (bot, name, spec, turns, by) =
        (bot.to_string(), name.to_string(), p.spec.clone(), serde_json::to_string(&*p.turns)?, p.created_by);
    db.call(move |c| {
        let count: i64 = c.query_row(
            "SELECT COUNT(*) FROM chat_prefixes WHERE bot = ?1 AND chat_id = ?2",
            params![bot, chat_id],
            |r| r.get(0),
        )?;
        if count >= MAX_PER_CHAT {
            return Ok(Inserted::Full);
        }
        let n = c.execute(
            "INSERT OR IGNORE INTO chat_prefixes(bot, chat_id, name, spec, turns, created_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![bot, chat_id, name, spec, turns, by, now_ms()],
        )?;
        Ok(if n > 0 { Inserted::Added } else { Inserted::Taken })
    })
    .await
}

pub async fn delete(db: &Db, bot: &str, chat_id: i64, name: &str) -> Result<bool> {
    let (bot, name) = (bot.to_string(), name.to_string());
    db.call(move |c| {
        Ok(c.execute(
            "DELETE FROM chat_prefixes WHERE bot = ?1 AND chat_id = ?2 AND name = ?3",
            params![bot, chat_id, name],
        )? > 0)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(check_name("Pirate2").unwrap(), "pirate2");
        for bad in ["ab", "a-b-c", "c$x", "名字很好", "abcdefghijklmnopq", "a b c"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(check_name("abc").is_ok() && check_name("abcdefghijklmnop").is_ok());
    }

    #[test]
    fn turns_from_marker_lines() {
        let t = turns_from_text("::system\nYou are a pirate.\n::user\nhi\n::assistant\nArr!").unwrap();
        assert_eq!(t.iter().map(|t| t.role).collect::<Vec<_>>(), vec![Role::System, Role::User, Role::Assistant]);
        assert!(t.iter().all(|t| t.origin == Origin::Synthetic));
        assert!(turns_from_text("::system\nonly a system prompt").is_ok());
        assert!(turns_from_text("no markers at all").is_err());
        assert!(turns_from_text("::system\nx\n::user\nhi").is_err(), "must end with the answer");
        assert!(turns_from_text("::user\nhi\n::system\nlate\n::assistant\nok").is_err());
        assert!(turns_from_text("::system\n\n::user\nhi\n::assistant\nok").is_err(), "empty system prompt");
    }
}
