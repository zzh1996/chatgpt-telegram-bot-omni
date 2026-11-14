//! Per-(bot, chat) scheduling of message operations.
//!
//! All operations of one bot in one chat go through a [`ChatGate`], one at a
//! time. A request says how urgent it is ([`Class`]) and what it costs
//! ([`Weight`]); the gate starts it only once Telegram's budget has room (see
//! [`flood`](super::flood)), so Telegram never needs to answer FLOOD_WAIT.
//! Among the requests that fit, the most urgent goes first: finished content,
//! then the first message of a new reply, then streaming updates, those by due
//! time (each reply sets its own pace: young replies often, long-running ones
//! less often). Streaming updates leave a reserve for the other two.
//!
//! A permit is only a slot: holders decide *what* to send after they get it,
//! so a slot always carries the newest content. If that content is heavier
//! than asked for, the holder checks [`GatePermit::fits`] and asks again.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::params;
use tokio::sync::Notify;
use tokio::time::Instant;
use tracing::warn;

use super::flood::{Budget, Reserve, Weight};
use crate::store::{Db, now_ms};

/// How urgent an operation is, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// Finished replies, command answers, notices.
    Final,
    /// The first message of a new reply.
    First,
    /// Updates of replies that are still being generated.
    Stream,
}

/// A request for a slot.
#[derive(Debug, Clone, Copy)]
pub struct Ask {
    pub class: Class,
    /// What the operation is expected to cost.
    pub weight: Weight,
    /// Not before this (a streaming reply's own pace); also orders streaming
    /// updates among themselves (earliest due first).
    pub not_before: Option<Instant>,
}

impl Ask {
    pub fn finals(weight: Weight) -> Self {
        Ask { class: Class::Final, weight, not_before: None }
    }
}

fn reserve(class: Class) -> Reserve {
    if class == Class::Stream { Reserve::STREAM } else { Reserve::NONE }
}

#[derive(Debug)]
struct Waiter {
    ask: Ask,
    arrived: Instant,
}

impl Waiter {
    /// Order among waiters that may start: class, then due time, then arrival.
    fn key(&self, seq: u64) -> (Class, Instant, u64) {
        (self.ask.class, self.ask.not_before.unwrap_or(self.arrived), seq)
    }
}

#[derive(Debug, Default)]
struct GateState {
    budget: Budget,
    busy: bool,
    seq: u64,
    waiting: BTreeMap<u64, Waiter>,
}

impl GateState {
    /// When a waiter could start: its own pace and the budget.
    fn start_of(&self, w: &Waiter, now: Instant, unlimited: bool) -> Instant {
        let at = w.ask.not_before.map_or(now, |t| t.max(now));
        if unlimited { at } else { self.budget.earliest(at, w.ask.weight, reserve(w.ask.class)) }
    }

    /// The waiter that should start now, if any, and else when to look again.
    fn pick(&self, now: Instant, unlimited: bool) -> (Option<u64>, Option<Instant>) {
        let mut best: Option<((Class, Instant, u64), u64)> = None;
        let mut next: Option<Instant> = None;
        for (seq, w) in &self.waiting {
            let start = self.start_of(w, now, unlimited);
            if start <= now {
                let key = w.key(*seq);
                if best.as_ref().is_none_or(|(k, _)| key < *k) {
                    best = Some((key, *seq));
                }
            } else if next.is_none_or(|n| start < n) {
                next = Some(start);
            }
        }
        (best.map(|(_, seq)| seq), next)
    }
}

/// Writes accepted operations to the database, so that a restart knows what
/// Telegram may still count.
#[derive(Clone)]
pub struct FloodLog {
    db: Db,
    bot: String,
}

impl FloodLog {
    fn write(&self, chat_id: i64, w: Weight) {
        let (db, bot, at, units) = (self.db.clone(), self.bot.clone(), now_ms(), w.units());
        tokio::spawn(async move {
            let r = db
                .call(move |c| {
                    c.execute(
                        "INSERT INTO flood_log(bot, chat_id, at, units) VALUES (?1, ?2, ?3, ?4)",
                        params![bot, chat_id, at, units],
                    )?;
                    c.execute("DELETE FROM flood_log WHERE at < ?1", params![at - 2 * 60_000])?;
                    Ok(())
                })
                .await;
            if let Err(e) = r {
                warn!(error = %format!("{e:#}"), "writing the flood log failed");
            }
        });
    }
}

#[derive(Clone)]
pub struct ChatGate {
    state: Arc<Mutex<GateState>>,
    notify: Arc<Notify>,
    /// Dev probes measuring Telegram's limits: no budget.
    unlimited: bool,
    log: Option<(FloodLog, i64)>,
}

/// Held while performing one operation. Dropping it records the operation
/// (with the weight set by the holder), unless it was unused or refused.
pub struct GatePermit {
    gate: ChatGate,
    class: Class,
    weight: AtomicU8,
    used: AtomicBool,
}

fn weight_code(w: Weight) -> u8 {
    match w {
        Weight::Free => 0,
        Weight::Light => 1,
        Weight::Heavy => 2,
    }
}

fn weight_of(c: u8) -> Weight {
    match c {
        0 => Weight::Free,
        1 => Weight::Light,
        _ => Weight::Heavy,
    }
}

impl GatePermit {
    /// Whether an operation of weight `w` fits now (the newest content may be
    /// heavier than what was asked for).
    pub fn fits(&self, w: Weight) -> bool {
        if self.gate.unlimited {
            return true;
        }
        let now = Instant::now();
        self.gate.state.lock().budget.earliest(now, w, reserve(self.class)) <= now
    }

    /// The operation about to be performed weighs `w`.
    pub fn set_weight(&self, w: Weight) {
        self.weight.store(weight_code(w), Ordering::Relaxed);
    }

    /// Telegram said FLOOD_WAIT anyway (the model missed something): the
    /// operation did not count, and nothing may be sent before the wait ends.
    pub fn flood_wait(&self, wait: Duration) {
        self.used.store(false, Ordering::Relaxed);
        self.gate.state.lock().budget.block_until(Instant::now() + wait + Duration::from_secs(1));
    }

    /// Nothing was sent with this permit.
    pub fn unused(&self) {
        self.used.store(false, Ordering::Relaxed);
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let w = weight_of(self.weight.load(Ordering::Relaxed));
        let counted = self.used.load(Ordering::Relaxed) && w != Weight::Free && !self.gate.unlimited;
        {
            let mut s = self.gate.state.lock();
            s.busy = false;
            if counted {
                // Telegram has answered: the latest moment it can have counted the operation.
                s.budget.record(Instant::now(), w);
            }
        }
        if counted && let Some((log, chat)) = &self.gate.log {
            log.write(*chat, w);
        }
        self.gate.notify.notify_waiters();
    }
}

/// A queued request; leaves the queue if the waiting future is dropped.
struct Waiting {
    gate: ChatGate,
    seq: u64,
    queued: bool,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.queued {
            self.gate.state.lock().waiting.remove(&self.seq);
            self.gate.notify.notify_waiters();
        }
    }
}

impl ChatGate {
    fn with(budget: Budget, unlimited: bool, log: Option<(FloodLog, i64)>) -> Self {
        Self {
            state: Arc::new(Mutex::new(GateState { budget, ..Default::default() })),
            notify: Arc::new(Notify::new()),
            unlimited,
            log,
        }
    }

    /// A gate that only serializes (dev probes that measure Telegram's limits).
    #[cfg(any(debug_assertions, test))]
    pub fn unlimited() -> Self {
        Self::with(Budget::default(), true, None)
    }

    /// Wait until `ask` may start. Cancel-safe: dropping the future leaves the queue.
    pub async fn acquire(&self, ask: Ask) -> GatePermit {
        let seq = {
            let mut s = self.state.lock();
            s.seq += 1;
            let seq = s.seq;
            s.waiting.insert(seq, Waiter { ask, arrived: Instant::now() });
            seq
        };
        let mut waiting = Waiting { gate: self.clone(), seq, queued: true };
        loop {
            // Register for wake-ups before looking at the state, so none is missed.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wake_at = {
                let mut s = self.state.lock();
                let now = Instant::now();
                let (best, next) = if s.busy { (None, None) } else { s.pick(now, self.unlimited) };
                if best == Some(seq) {
                    s.busy = true;
                    s.waiting.remove(&seq);
                    waiting.queued = false;
                    return GatePermit {
                        gate: self.clone(),
                        class: ask.class,
                        weight: AtomicU8::new(weight_code(ask.weight)),
                        used: AtomicBool::new(true),
                    };
                }
                // Someone else may start now: every waiter sleeps until the earliest start
                // of all, and a finished operation wakes everyone.
                next
            };
            match wake_at {
                Some(t) => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(t) => {}
                        _ = notified.as_mut() => {}
                    }
                }
                None => notified.as_mut().await,
            }
        }
    }
}

/// All gates of one bot, keyed by chat id.
#[derive(Clone)]
pub struct Gates {
    map: Arc<Mutex<HashMap<i64, ChatGate>>>,
    log: Option<FloodLog>,
}

impl Gates {
    /// Gates that remember nothing across restarts (tests).
    pub fn new() -> Self {
        Self { map: Arc::new(Mutex::new(HashMap::new())), log: None }
    }

    /// Gates that persist accepted operations and start from those of the last
    /// minute (Telegram may still count them after a restart).
    pub async fn load(db: &Db, bot: &str) -> anyhow::Result<Self> {
        let (bot2, since) = (bot.to_string(), now_ms() - crate::tg::flood::LONG.as_millis() as i64);
        let rows: Vec<(i64, i64, u32)> = db
            .call(move |c| {
                let mut s =
                    c.prepare("SELECT chat_id, at, units FROM flood_log WHERE bot = ?1 AND at >= ?2 ORDER BY at")?;
                let v = s
                    .query_map(params![bot2, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(v)
            })
            .await?;
        let log = FloodLog { db: db.clone(), bot: bot.to_string() };
        let (now, now_wall) = (Instant::now(), now_ms());
        let mut map = HashMap::new();
        for (chat, at, units) in rows {
            let gate =
                map.entry(chat).or_insert_with(|| ChatGate::with(Budget::default(), false, Some((log.clone(), chat))));
            let ago = Duration::from_millis((now_wall - at).max(0) as u64);
            gate.state.lock().budget.record(now.checked_sub(ago).unwrap_or(now), Weight::from_units(units));
        }
        Ok(Self { map: Arc::new(Mutex::new(map)), log: Some(log) })
    }

    pub fn get(&self, chat_id: i64) -> ChatGate {
        let mut m = self.map.lock();
        m.entry(chat_id)
            .or_insert_with(|| ChatGate::with(Budget::default(), false, self.log.clone().map(|l| (l, chat_id))))
            .clone()
    }
}

impl Default for Gates {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(weight: Weight, not_before: Option<Instant>) -> Ask {
        Ask { class: Class::Stream, weight, not_before }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_the_budget_instead_of_flooding() {
        let g = ChatGate::with(Budget::default(), false, None);
        let t0 = Instant::now();
        // Two heavy operations: the second only after the first left the 5 s window.
        drop(g.acquire(Ask::finals(Weight::Heavy)).await);
        drop(g.acquire(Ask::finals(Weight::Heavy)).await);
        assert!(Instant::now() - t0 >= crate::tg::flood::SHORT);
        // A flood wait blocks everything.
        let p = g.acquire(Ask::finals(Weight::Light)).await;
        p.flood_wait(Duration::from_secs(30));
        drop(p);
        let t1 = Instant::now();
        drop(g.acquire(Ask::finals(Weight::Free)).await);
        assert!(Instant::now() - t1 >= Duration::from_secs(31));
    }

    #[tokio::test(start_paused = true)]
    async fn unused_and_refused_operations_cost_nothing() {
        let g = ChatGate::with(Budget::default(), false, None);
        for _ in 0..10 {
            g.acquire(Ask::finals(Weight::Heavy)).await.unused();
        }
        let t0 = Instant::now();
        drop(g.acquire(Ask::finals(Weight::Heavy)).await);
        assert_eq!(Instant::now(), t0);
    }

    #[tokio::test(start_paused = true)]
    async fn finals_before_new_replies_before_streams() {
        let g = ChatGate::with(Budget::default(), false, None);
        let first = g.acquire(Ask::finals(Weight::Light)).await;
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        for (name, ask) in [
            ("stream", stream(Weight::Light, None)),
            ("first", Ask { class: Class::First, weight: Weight::Light, not_before: None }),
            ("final", Ask::finals(Weight::Light)),
        ] {
            let (g, order) = (g.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let p = g.acquire(ask).await;
                order.lock().push(name);
                drop(p);
            }));
            tokio::task::yield_now().await;
        }
        drop(first);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock(), vec!["final", "first", "stream"]);
    }

    #[tokio::test(start_paused = true)]
    async fn streams_go_by_their_due_time_and_leave_a_reserve() {
        let g = ChatGate::with(Budget::default(), false, None);
        let t0 = Instant::now();
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut tasks = Vec::new();
        // An old reply due in 2 s, a young one due now.
        for (name, due) in [("old", t0 + Duration::from_secs(2)), ("young", t0)] {
            let (g, order) = (g.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let p = g.acquire(stream(Weight::Light, Some(due))).await;
                order.lock().push((name, Instant::now() - t0));
                drop(p);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        let order = order.lock().clone();
        assert_eq!(order[0], ("young", Duration::ZERO));
        assert_eq!(order[1], ("old", Duration::from_secs(2)));
        // Streams never use the last operation of a 5 s window.
        for _ in 0..4 {
            drop(g.acquire(stream(Weight::Light, None)).await);
        }
        let t1 = Instant::now();
        drop(g.acquire(Ask::finals(Weight::Light)).await);
        assert_eq!(Instant::now(), t1, "the reserve is there for finished content");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_waiter_leaves_the_queue() {
        let g = ChatGate::with(Budget::default(), false, None);
        let p = g.acquire(Ask::finals(Weight::Light)).await;
        let w = tokio::time::timeout(Duration::from_millis(10), g.acquire(Ask::finals(Weight::Light))).await;
        assert!(w.is_err());
        drop(p);
        tokio::time::timeout(Duration::from_millis(10), g.acquire(stream(Weight::Light, None)))
            .await
            .expect("not blocked by the cancelled waiter");
    }

    #[tokio::test(start_paused = true)]
    async fn unlimited_gates_only_serialize() {
        let g = ChatGate::unlimited();
        let t0 = Instant::now();
        for _ in 0..30 {
            drop(g.acquire(Ask::finals(Weight::Heavy)).await);
        }
        assert_eq!(Instant::now(), t0);
    }
}
