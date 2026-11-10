//! Per-(bot, chat) rate limiting of message operations.
//!
//! Telegram limits how often a bot may send/edit in one chat (roughly 20
//! operations per minute in groups). All operations of one bot in one chat go
//! through a [`ChatGate`], which serializes them and enforces a minimum
//! interval. A FLOOD_WAIT returned by Telegram blocks the gate for the
//! requested time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tokio::time::Instant;

#[derive(Debug)]
struct GateState {
    next_allowed: Instant,
}

#[derive(Clone)]
pub struct ChatGate {
    lock: Arc<AsyncMutex<()>>,
    state: Arc<Mutex<GateState>>,
    interval: Duration,
}

/// Held while performing one operation; the next operation may start
/// `interval` after this guard is dropped (or after a flood wait).
pub struct GatePermit {
    _guard: OwnedMutexGuard<()>,
    state: Arc<Mutex<GateState>>,
    interval: Duration,
}

impl GatePermit {
    /// Telegram told us to wait: block the gate for `wait` from now.
    pub fn flood_wait(&self, wait: Duration) {
        let mut s = self.state.lock();
        let until = Instant::now() + wait;
        if until > s.next_allowed {
            s.next_allowed = until;
        }
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let mut s = self.state.lock();
        let next = Instant::now() + self.interval;
        if next > s.next_allowed {
            s.next_allowed = next;
        }
    }
}

impl ChatGate {
    pub fn new(interval: Duration) -> Self {
        Self {
            lock: Arc::new(AsyncMutex::new(())),
            state: Arc::new(Mutex::new(GateState { next_allowed: Instant::now() })),
            interval,
        }
    }

    pub async fn acquire(&self) -> GatePermit {
        let guard = self.lock.clone().lock_owned().await;
        loop {
            let wait_until = self.state.lock().next_allowed;
            if wait_until <= Instant::now() {
                break;
            }
            tokio::time::sleep_until(wait_until).await;
        }
        GatePermit { _guard: guard, state: self.state.clone(), interval: self.interval }
    }
}

/// All gates of one bot, keyed by chat id.
#[derive(Clone)]
pub struct Gates {
    map: Arc<Mutex<HashMap<i64, ChatGate>>>,
    group_interval: Duration,
    private_interval: Duration,
}

impl Gates {
    pub fn new(group_interval: Duration, private_interval: Duration) -> Self {
        Self { map: Arc::new(Mutex::new(HashMap::new())), group_interval, private_interval }
    }

    pub fn get(&self, chat_id: i64) -> ChatGate {
        let mut m = self.map.lock();
        m.entry(chat_id)
            .or_insert_with(|| ChatGate::new(if chat_id > 0 { self.private_interval } else { self.group_interval }))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn enforces_interval_and_flood_wait() {
        let g = ChatGate::new(Duration::from_secs(3));
        let t0 = Instant::now();
        drop(g.acquire().await);
        drop(g.acquire().await);
        assert!(Instant::now() - t0 >= Duration::from_secs(3));
        let p = g.acquire().await;
        p.flood_wait(Duration::from_secs(30));
        drop(p);
        let t1 = Instant::now();
        drop(g.acquire().await);
        assert!(Instant::now() - t1 >= Duration::from_secs(29));
    }
}
