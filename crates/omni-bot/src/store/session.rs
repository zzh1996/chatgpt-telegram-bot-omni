//! grammers [`Session`] backed by our SQLite database (one logical session per bot).
//!
//! The update state (pts/qts) is written by the intake loop only after every
//! update carrying that state has been durably recorded (see `intake.rs`), so a
//! restart can re-deliver processed updates (deduplicated) but never skip one.

use std::collections::HashMap;

use grammers_session::types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use parking_lot::RwLock;
use rusqlite::{Connection, OptionalExtension, params};

use super::Db;

#[derive(Debug, thiserror::Error)]
#[error("session storage error: {0}")]
pub struct SessionError(String);

impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        SessionError(format!("{e:#}"))
    }
}

pub struct BotSession {
    bot: String,
    db: Db,
    home_dc: RwLock<i32>,
    dc_options: RwLock<HashMap<i32, DcOption>>,
    peers: RwLock<HashMap<i64, PeerInfo>>,
}

fn peer_key(id: PeerId) -> i64 {
    // PeerId serializes transparently as its internal i64 (Bot API dialog id or self sentinel).
    serde_json::to_value(id).ok().and_then(|v| v.as_i64()).unwrap_or_default()
}

impl BotSession {
    /// Load the session for `bot` from the database (creating defaults if absent).
    pub fn load(db: Db, bot: &str) -> anyhow::Result<Self> {
        let defaults = SessionData::default();
        let bot_s = bot.to_string();
        let (home_dc, dcs, peers) = db.call_sync(move |c| {
            let home: Option<i32> = c
                .query_row("SELECT home_dc FROM tg_session WHERE bot = ?1", params![bot_s], |r| r.get(0))
                .optional()?;
            let mut stmt = c.prepare("SELECT option FROM tg_dc WHERE bot = ?1")?;
            let dcs: Vec<String> = stmt.query_map(params![bot_s], |r| r.get(0))?.collect::<Result<_, _>>()?;
            let mut stmt = c.prepare("SELECT info FROM tg_peer WHERE bot = ?1")?;
            let peers: Vec<String> = stmt.query_map(params![bot_s], |r| r.get(0))?.collect::<Result<_, _>>()?;
            Ok((home, dcs, peers))
        })?;
        let mut dc_options = defaults.dc_options.clone();
        for s in dcs {
            let o: DcOption = serde_json::from_str(&s)?;
            dc_options.insert(o.id, o);
        }
        let mut peer_map = HashMap::new();
        for s in peers {
            let p: PeerInfo = serde_json::from_str(&s)?;
            peer_map.insert(peer_key(p.id()), p);
        }
        Ok(Self {
            bot: bot.to_string(),
            db,
            home_dc: RwLock::new(home_dc.unwrap_or(defaults.home_dc)),
            dc_options: RwLock::new(dc_options),
            peers: RwLock::new(peer_map),
        })
    }

    /// Persist an update-state change inside an existing transaction.
    /// Used by update intake to commit "message recorded" and "pts advanced" atomically.
    pub fn apply_update_state_in(conn: &Connection, bot: &str, update: &UpdateState) -> rusqlite::Result<()> {
        match update {
            UpdateState::All(s) => {
                conn.execute(
                    "INSERT INTO tg_update_state(bot, pts, qts, date, seq) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(bot) DO UPDATE SET pts = excluded.pts, qts = excluded.qts, date = excluded.date, seq = excluded.seq",
                    params![bot, s.pts, s.qts, s.date, s.seq],
                )?;
                conn.execute("DELETE FROM tg_channel_state WHERE bot = ?1", params![bot])?;
                for ch in &s.channels {
                    conn.execute(
                        "INSERT INTO tg_channel_state(bot, channel_id, pts) VALUES (?1, ?2, ?3)",
                        params![bot, ch.id, ch.pts],
                    )?;
                }
            }
            UpdateState::Primary { pts, date, seq } => {
                conn.execute(
                    "INSERT INTO tg_update_state(bot, pts, qts, date, seq) VALUES (?1, ?2, 0, ?3, ?4)
                     ON CONFLICT(bot) DO UPDATE SET pts = excluded.pts, date = excluded.date, seq = excluded.seq",
                    params![bot, pts, date, seq],
                )?;
            }
            UpdateState::Secondary { qts } => {
                conn.execute(
                    "INSERT INTO tg_update_state(bot, pts, qts, date, seq) VALUES (?1, 0, ?2, 0, 0)
                     ON CONFLICT(bot) DO UPDATE SET qts = excluded.qts",
                    params![bot, qts],
                )?;
            }
            UpdateState::Channel { id, pts } => {
                conn.execute(
                    "INSERT INTO tg_channel_state(bot, channel_id, pts) VALUES (?1, ?2, ?3)
                     ON CONFLICT(bot, channel_id) DO UPDATE SET pts = excluded.pts",
                    params![bot, id, pts],
                )?;
            }
        }
        Ok(())
    }

    pub fn load_updates_state(conn: &Connection, bot: &str) -> rusqlite::Result<UpdatesState> {
        let row: Option<(i32, i32, i32, i32)> = conn
            .query_row("SELECT pts, qts, date, seq FROM tg_update_state WHERE bot = ?1", params![bot], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let mut stmt = conn.prepare("SELECT channel_id, pts FROM tg_channel_state WHERE bot = ?1")?;
        let channels = stmt
            .query_map(params![bot], |r| Ok(ChannelState { id: r.get(0)?, pts: r.get(1)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        let (pts, qts, date, seq) = row.unwrap_or_default();
        Ok(UpdatesState { pts, qts, date, seq, channels })
    }
}

impl Session for BotSession {
    type Error = SessionError;

    fn home_dc_id(&self) -> Result<i32, SessionError> {
        Ok(*self.home_dc.read())
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), SessionError>> {
        *self.home_dc.write() = dc_id;
        let (db, bot) = (self.db.clone(), self.bot.clone());
        Box::pin(async move {
            db.call(move |c| {
                c.execute(
                    "INSERT INTO tg_session(bot, home_dc) VALUES (?1, ?2)
                     ON CONFLICT(bot) DO UPDATE SET home_dc = excluded.home_dc",
                    params![bot, dc_id],
                )?;
                Ok(())
            })
            .await?;
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, SessionError> {
        Ok(self.dc_options.read().get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), SessionError>> {
        self.dc_options.write().insert(dc_option.id, dc_option.clone());
        let (db, bot) = (self.db.clone(), self.bot.clone());
        let id = dc_option.id;
        let json = serde_json::to_string(dc_option).unwrap_or_default();
        Box::pin(async move {
            db.call(move |c| {
                c.execute(
                    "INSERT INTO tg_dc(bot, dc_id, option) VALUES (?1, ?2, ?3)
                     ON CONFLICT(bot, dc_id) DO UPDATE SET option = excluded.option",
                    params![bot, id, json],
                )?;
                Ok(())
            })
            .await?;
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, SessionError>> {
        let r = self.peers.read().get(&peer_key(peer)).cloned();
        Box::pin(async move { Ok(r) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), SessionError>> {
        let key = peer_key(peer.id());
        let merged = {
            let mut map = self.peers.write();
            match map.get_mut(&key) {
                Some(existing) => {
                    let mut candidate = existing.clone();
                    let changed = candidate.extend_info(&peer) || candidate != *existing;
                    if changed {
                        *existing = candidate.clone();
                        Some(candidate)
                    } else {
                        None
                    }
                }
                None => {
                    map.insert(key, peer.clone());
                    Some(peer)
                }
            }
        };
        let (db, bot) = (self.db.clone(), self.bot.clone());
        Box::pin(async move {
            if let Some(p) = merged {
                let json = serde_json::to_string(&p).unwrap_or_default();
                db.call(move |c| {
                    c.execute(
                        "INSERT INTO tg_peer(bot, peer_id, info) VALUES (?1, ?2, ?3)
                         ON CONFLICT(bot, peer_id) DO UPDATE SET info = excluded.info",
                        params![bot, key, json],
                    )?;
                    Ok(())
                })
                .await?;
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, SessionError>> {
        let (db, bot) = (self.db.clone(), self.bot.clone());
        Box::pin(async move { Ok(db.call(move |c| Ok(BotSession::load_updates_state(c, &bot)?)).await?) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), SessionError>> {
        let (db, bot) = (self.db.clone(), self.bot.clone());
        Box::pin(async move {
            db.call(move |c| {
                BotSession::apply_update_state_in(c, &bot, &update)?;
                Ok(())
            })
            .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn persists_state_and_peers() {
        let db = Db::open_in_memory().unwrap();
        let s = BotSession::load(db.clone(), "b1").unwrap();
        s.set_home_dc_id(4).await.unwrap();
        s.set_update_state(UpdateState::All(UpdatesState {
            pts: 10,
            qts: 2,
            date: 100,
            seq: 1,
            channels: vec![ChannelState { id: 55, pts: 7 }],
        }))
        .await
        .unwrap();
        s.set_update_state(UpdateState::Channel { id: 55, pts: 8 }).await.unwrap();
        s.set_update_state(UpdateState::Primary { pts: 11, date: 101, seq: 2 }).await.unwrap();
        s.cache_peer(PeerInfo::User { id: 42, auth: None, bot: Some(false), is_self: None }).await.unwrap();

        let s2 = BotSession::load(db.clone(), "b1").unwrap();
        assert_eq!(s2.home_dc_id().unwrap(), 4);
        let st = s2.updates_state().await.unwrap();
        assert_eq!((st.pts, st.qts, st.date, st.seq), (11, 2, 101, 2));
        assert_eq!(st.channels, vec![ChannelState { id: 55, pts: 8 }]);
        let p = s2.peer(PeerId::user(42).unwrap()).await.unwrap();
        assert!(matches!(p, Some(PeerInfo::User { id: 42, .. })));
        // other bots are isolated
        let other = BotSession::load(db, "b2").unwrap();
        assert_eq!(other.updates_state().await.unwrap(), UpdatesState::default());
    }
}
