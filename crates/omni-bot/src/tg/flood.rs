//! Telegram's flood control for a bot's messages in one chat, and the bot's
//! accounting of it: every operation is weighed and waits *before* it is
//! sent, so that Telegram never answers FLOOD_WAIT.
//!
//! Measured on 2026-11-11 (docs/research/telegram-flood.md,
//! tools/telegram-rate-model.py), per bot and chat: an accepted send or edit
//! is light (1 unit) or heavy (16); deletes and refused operations are free.
//! Any 5 s hold at most 5 operations, of which at most 1 heavy; a fixed 60 s
//! window at most 20 operations and 80 units. Heavy are: a send of ≥ ~2110
//! code points; an edit that adds ≥ ~2110 to a message of less than ~5000–7000;
//! an edit of a message with 2–3 top-level blocks other than details blocks
//! whose text before and after is ≥ ~8210; an edit of a message of more than
//! ~20000–30000 before and after, whatever its blocks.
//!
//! The bot applies the same rules with margins: lower thresholds, longer
//! windows, both readings of the block-count rule, and a *sliding* 60 s
//! window. Every operation Telegram counts in its fixed window lies within the
//! 60 s before ours, so the sliding count bounds it wherever Telegram's window
//! starts. A past operation is timed by when its answer arrived, the latest
//! moment Telegram can have counted it.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;

use super::outbox::Content;

/// How much of a chat's budget an operation takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Weight {
    /// Deletes, and anything Telegram does not count.
    Free,
    Light,
    Heavy,
}

impl Weight {
    pub fn units(self) -> u32 {
        match self {
            Weight::Free => 0,
            Weight::Light => LIGHT_UNITS,
            Weight::Heavy => HEAVY_UNITS,
        }
    }

    pub fn from_units(u: u32) -> Self {
        match u {
            0 => Weight::Free,
            u if u >= HEAVY_UNITS => Weight::Heavy,
            _ => Weight::Light,
        }
    }
}

/// What Telegram's flood control looks at in a message: the code points of all
/// its text, and its top-level blocks other than details (collapsible) blocks.
/// The block count is recorded but not used: whether big edits are heavy
/// depends on the layout in a way not yet understood (see [`BIG`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub chars: usize,
    pub top_blocks: usize,
}

impl Shape {
    pub fn of(content: &Content) -> Self {
        use omni_markdown::rich::RBlock;
        match content {
            Content::Rich(blocks) => Shape {
                chars: super::rich::expected_text(blocks).chars().filter(|c| *c != '\u{1}').count(),
                top_blocks: blocks.iter().filter(|b| !matches!(b, RBlock::Details { .. })).count(),
            },
            Content::Text(f) => Shape { chars: f.text.chars().count(), top_blocks: 1 },
        }
    }
}

/// A send at least this big is heavy (measured: 2101..=2120).
pub const SEND_HEAVY: usize = 2000;
/// An edit that adds at least this much (measured: 2101..=2130)…
pub const GROW_HEAVY: usize = 2000;
/// …to a message smaller than this is heavy (measured: 5001..=7000).
pub const GROW_FROM_BELOW: usize = 7500;
/// An edit of a message this big before and after is heavy (measured from
/// 8195..=8225 on). Some layouts stay light up to 20000 (plain text, 4+
/// paragraphs, a details block with one footer), others do not (2-3 blocks,
/// header + details + footers, two details blocks), so every big edit counts
/// as heavy until the rule is understood: slower updates of long replies, never
/// a FLOOD_WAIT.
pub const BIG: usize = 8000;

pub fn send_weight(new: Shape) -> Weight {
    if new.chars >= SEND_HEAVY { Weight::Heavy } else { Weight::Light }
}

/// `old`: what Telegram shows before the edit (unknown: as heavy as a send).
pub fn edit_weight(old: Option<Shape>, new: Shape) -> Weight {
    let Some(old) = old else { return send_weight(new) };
    let grows = new.chars >= old.chars + GROW_HEAVY && old.chars < GROW_FROM_BELOW;
    let big = old.chars >= BIG && new.chars >= BIG;
    if grows || big { Weight::Heavy } else { Weight::Light }
}

/// Any operation whose answer arrived within this before ours is in the same
/// 5 s window (5 s measured with millisecond precision, plus a margin).
pub const SHORT: Duration = Duration::from_millis(5_400);
/// Likewise for the 60 s window.
pub const LONG: Duration = Duration::from_millis(61_000);
const SHORT_OPS: usize = 5;
const SHORT_HEAVY: usize = 1;
const LONG_OPS: usize = 20;
const LONG_UNITS: u32 = 80;
const LIGHT_UNITS: u32 = 1;
const HEAVY_UNITS: u32 = 16;

/// Room an operation must leave for more urgent ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserve {
    pub short_ops: usize,
    pub long_ops: usize,
    pub long_units: u32,
}

impl Reserve {
    pub const NONE: Reserve = Reserve { short_ops: 0, long_ops: 0, long_units: 0 };
    /// What streaming updates leave for finished replies and new ones: one
    /// operation in any 5 s, and one heavy plus one light operation per minute.
    pub const STREAM: Reserve = Reserve { short_ops: 1, long_ops: 2, long_units: HEAVY_UNITS + LIGHT_UNITS };
}

/// The operations of one bot in one chat that Telegram may still count.
#[derive(Debug, Default)]
pub struct Budget {
    /// (when the answer arrived, weight), oldest first; free operations are not kept.
    done: VecDeque<(Instant, Weight)>,
    /// After an unexpected FLOOD_WAIT: nothing before this.
    blocked_until: Option<Instant>,
}

impl Budget {
    /// Telegram accepted an operation (or may have: count it when unsure).
    pub fn record(&mut self, at: Instant, w: Weight) {
        if w == Weight::Free {
            return;
        }
        let i = self.done.partition_point(|(t, _)| *t <= at);
        self.done.insert(i, (at, w));
        self.prune(at);
    }

    /// Telegram said FLOOD_WAIT after all: the model missed something, so
    /// stop until then.
    pub fn block_until(&mut self, until: Instant) {
        if self.blocked_until.is_none_or(|b| b < until) {
            self.blocked_until = Some(until);
        }
    }

    /// The earliest moment, not before `now`, at which an operation of weight
    /// `w` that leaves `reserve` fits.
    pub fn earliest(&self, now: Instant, w: Weight, reserve: Reserve) -> Instant {
        let now = self.blocked_until.map_or(now, |b| b.max(now));
        if w == Weight::Free {
            return now;
        }
        // Room only grows as past operations leave a window: try now, then each such moment.
        let mut times: Vec<Instant> = self
            .done
            .iter()
            .flat_map(|(t, _)| [*t + SHORT, *t + LONG])
            .filter(|t| *t > now)
            .collect();
        times.sort();
        std::iter::once(now).chain(times).find(|t| self.fits(*t, w, reserve)).unwrap_or(now + LONG)
    }

    fn fits(&self, t: Instant, w: Weight, r: Reserve) -> bool {
        let (mut short_ops, mut short_heavy, mut long_ops, mut long_units) = (0, 0, 0, 0);
        for (at, ow) in &self.done {
            if *at + LONG > t {
                long_ops += 1;
                long_units += ow.units();
            }
            if *at + SHORT > t {
                short_ops += 1;
                short_heavy += usize::from(*ow == Weight::Heavy);
            }
        }
        short_ops + 1 + r.short_ops <= SHORT_OPS
            && (w != Weight::Heavy || short_heavy < SHORT_HEAVY)
            && long_ops + 1 + r.long_ops <= LONG_OPS
            && long_units + w.units() + r.long_units <= LONG_UNITS
    }

    fn prune(&mut self, now: Instant) {
        while self.done.front().is_some_and(|(t, _)| *t + LONG <= now) {
            self.done.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(chars: usize, top_blocks: usize) -> Shape {
        Shape { chars, top_blocks }
    }

    #[test]
    fn weights_follow_the_measured_rules_with_margins() {
        use Weight::*;
        assert_eq!(send_weight(shape(1999, 3)), Light);
        assert_eq!(send_weight(shape(2000, 1)), Heavy);
        // Growth: adds ≥ 2000 to a message under 7500.
        assert_eq!(edit_weight(Some(shape(500, 3)), shape(2499, 3)), Light);
        assert_eq!(edit_weight(Some(shape(500, 3)), shape(2500, 3)), Heavy);
        assert_eq!(edit_weight(Some(shape(7499, 5)), shape(9499, 5)), Heavy);
        assert_eq!(edit_weight(Some(shape(7500, 5)), shape(12000, 5)), Light);
        // Size: both ≥ 8000, whatever the blocks.
        assert_eq!(edit_weight(Some(shape(8000, 3)), shape(8000, 3)), Heavy);
        assert_eq!(edit_weight(Some(shape(9000, 0)), shape(9000, 0)), Heavy);
        assert_eq!(edit_weight(Some(shape(9000, 4)), shape(9000, 4)), Heavy);
        assert_eq!(edit_weight(Some(shape(19999, 1)), shape(19999, 1)), Heavy);
        assert_eq!(edit_weight(Some(shape(30000, 5)), shape(30000, 5)), Heavy);
        assert_eq!(edit_weight(Some(shape(7999, 3)), shape(7999, 3)), Light);
        assert_eq!(edit_weight(Some(shape(7999, 3)), shape(9000, 3)), Light);
        assert_eq!(edit_weight(Some(shape(20000, 3)), shape(500, 3)), Light);
        // Unknown before: as heavy as a send.
        assert_eq!(edit_weight(None, shape(1999, 3)), Light);
        assert_eq!(edit_weight(None, shape(2000, 9)), Heavy);
    }

    #[test]
    fn details_blocks_do_not_count_as_blocks() {
        use omni_markdown::rich::{RBlock, RText};
        let p = |n: usize| RBlock::Paragraph(RText::plain("x".repeat(n)));
        let details = RBlock::Details { open: true, title: RText::plain("💭 Reasoning"), blocks: vec![p(9000)] };
        let reply = Content::Rich(vec![p(20), details.clone(), RBlock::Footer(RText::plain("2.1s"))]);
        assert_eq!(Shape::of(&reply), Shape { chars: 20 + 11 + 9000 + 4, top_blocks: 2 });
        assert_eq!(Shape::of(&Content::Rich(vec![details])).top_blocks, 0);
    }

    #[test]
    fn short_window_five_ops_one_heavy() {
        let t0 = Instant::now();
        let mut b = Budget::default();
        for i in 0..5 {
            let at = t0 + Duration::from_millis(200 * i);
            assert_eq!(b.earliest(at, Weight::Light, Reserve::NONE), at);
            b.record(at, Weight::Light);
        }
        // The 6th waits until the first leaves the 5 s window.
        let now = t0 + Duration::from_secs(1);
        assert_eq!(b.earliest(now, Weight::Light, Reserve::NONE), t0 + SHORT);
        // Two heavy operations are never in one 5 s window.
        let mut b = Budget::default();
        b.record(t0, Weight::Heavy);
        assert_eq!(b.earliest(t0, Weight::Light, Reserve::NONE), t0);
        assert_eq!(b.earliest(t0, Weight::Heavy, Reserve::NONE), t0 + SHORT);
    }

    #[test]
    fn long_window_twenty_ops_or_five_heavy() {
        let t0 = Instant::now();
        let step = Duration::from_secs(2);
        // 20 light per minute.
        let mut b = Budget::default();
        for i in 0..20 {
            b.record(t0 + step * i, Weight::Light);
        }
        let now = t0 + step * 20;
        assert_eq!(b.earliest(now, Weight::Light, Reserve::NONE), t0 + LONG);
        // 4 heavy + 16 light fit; a 5th heavy only into an empty minute.
        let mut b = Budget::default();
        let step = Duration::from_secs(6);
        for i in 0..4 {
            b.record(t0 + step * i, Weight::Heavy);
        }
        let now = t0 + Duration::from_secs(30);
        assert_eq!(b.earliest(now, Weight::Heavy, Reserve::NONE), now);
        b.record(now + Duration::from_secs(1), Weight::Light);
        // 4 heavy + 1 light: a 5th heavy (80 units) no longer fits until the first leaves.
        assert_eq!(b.earliest(now + Duration::from_secs(2), Weight::Heavy, Reserve::NONE), t0 + LONG);
    }

    #[test]
    fn streams_leave_a_reserve() {
        let t0 = Instant::now();
        let mut b = Budget::default();
        for i in 0..18 {
            b.record(t0 + Duration::from_secs(3 * i), Weight::Light);
        }
        let now = t0 + Duration::from_secs(54);
        // A stream update must leave 2 operations of the minute for finished replies…
        assert!(b.earliest(now, Weight::Light, Reserve::STREAM) > now);
        // …which may still go now.
        assert_eq!(b.earliest(now, Weight::Light, Reserve::NONE), now);
        // And it leaves one operation in any 5 s.
        let mut b = Budget::default();
        for i in 0..4 {
            b.record(t0 + Duration::from_millis(500 * i), Weight::Light);
        }
        let now = t0 + Duration::from_secs(2);
        assert!(b.earliest(now, Weight::Light, Reserve::STREAM) > now);
        assert_eq!(b.earliest(now, Weight::Light, Reserve::NONE), now);
    }

    #[test]
    fn a_flood_wait_blocks_everything_until_it_ends() {
        let t0 = Instant::now();
        let mut b = Budget::default();
        b.block_until(t0 + Duration::from_secs(7));
        assert_eq!(b.earliest(t0, Weight::Free, Reserve::NONE), t0 + Duration::from_secs(7));
        assert_eq!(b.earliest(t0, Weight::Light, Reserve::NONE), t0 + Duration::from_secs(7));
    }
}

/// Every operation recorded in docs/research/telegram-flood (the experiments
/// behind this model), replayed with the bot's own weights and budget: each one
/// Telegram refused must be one the bot would have held back.
#[cfg(test)]
mod replay {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use omni_markdown::rich::{RBlock, RText};

    use super::*;

    struct Rec {
        file: String,
        test: String,
        n: usize,
        slot: String,
        op: String,
        content: Option<Content>,
        /// Timeline (wall-clock reports share one) and start / answer in ms on it.
        timeline: String,
        start: i64,
        answered: i64,
        refused: bool,
    }

    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/research/telegram-flood")
    }

    fn load() -> Vec<Rec> {
        let rate = regex::Regex::new(
            r"^\s+#(?P<n>\d+)\s+(?P<slot>\S+)\s+(?P<op>send|edit|delete)\s+(?P<chars>\d+) chars\s+\d+ B(?:\s+(?P<cp>\d+) cp)?(?:\s+\d+ top)? (?P<script>\S+)\s+at\s+[\d.]+s start\s+(?P<start>[\d.]+)s(?: wall (?P<wall>\d+))? took\s+(?P<took>\d+)ms\s+(?P<verdict>.*)$",
        )
        .unwrap();
        let series = regex::Regex::new(r"^(\S+)\s+op\s+(\d+) size\s+(\d+)\s+t=\s*([\d.]+)s\s+took\s+(\d+)ms\s+(.*)$").unwrap();
        let blocks: HashMap<String, HashMap<String, Vec<RBlock>>> =
            serde_json::from_str(&std::fs::read_to_string(dir().join("rate18-blocks.json")).unwrap()).unwrap();
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "txt"))
            .collect();
        files.sort();
        let mut out = Vec::new();
        for path in files {
            let file = path.file_name().unwrap().to_string_lossy().to_string();
            let mut test = String::new();
            let mut series_sent: HashMap<String, bool> = HashMap::new();
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                if let Some(t) = line.strip_prefix("== ") {
                    test = t.to_string();
                } else if let Some(c) = rate.captures(line) {
                    let verdict = &c["verdict"];
                    if verdict.starts_with("ERROR") {
                        continue;
                    }
                    let (n, slot, op, chars, script): (usize, String, String, usize, &str) = (
                        c["n"].parse().unwrap(),
                        c["slot"].to_string(),
                        c["op"].to_string(),
                        c["chars"].parse().unwrap(),
                        c.name("script").unwrap().as_str(),
                    );
                    let content = if script == "blocks" {
                        match blocks.get(&test) {
                            Some(b) => Some(Content::Rich(b[&n.to_string()].clone())),
                            // Layouts not kept (rate19): the recorded size is all the weights use.
                            None => {
                                let cp: usize = c["cp"].parse().unwrap();
                                Some(Content::Rich(vec![RBlock::Paragraph(RText::plain("x".repeat(cp)))]))
                            }
                        }
                    } else if op == "delete" {
                        None
                    } else {
                        let label = test.trim_end_matches(" (killed run)");
                        Some(crate::devinject::script_content(&format!("{label} · {slot}"), n, chars, script))
                    };
                    let start_s: f64 = c["start"].parse().unwrap();
                    let took: i64 = c["took"].parse().unwrap();
                    let (timeline, start) = match c.name("wall") {
                        Some(w) => ("wall".to_string(), w.as_str().parse().unwrap()),
                        None => (format!("{file}/{test}"), (start_s * 1000.0) as i64),
                    };
                    out.push(Rec { file: file.clone(), test: test.clone(), n, slot, op, content, timeline, start, answered: start + took, refused: verdict.starts_with("FLOOD") });
                } else if let Some(c) = series.captures(line) {
                    let (label, n, size): (String, usize, usize) = (c[1].to_string(), c[2].parse().unwrap(), c[3].parse().unwrap());
                    let start = (c[4].parse::<f64>().unwrap() * 1000.0) as i64;
                    let refused = c[6].contains("FLOOD");
                    let sent = series_sent.entry(label.clone()).or_default();
                    let op = if *sent { "edit" } else { "send" }.to_string();
                    if !refused {
                        *sent = true;
                    }
                    let content = Some(crate::devinject::series_content(&label, n, size));
                    out.push(Rec { file: file.clone(), test: format!("series {label}"), n, slot: "A".into(), op, content, timeline: format!("{file}/{label}"), start, answered: start + c[5].parse::<i64>().unwrap(), refused });
                }
            }
        }
        out
    }

    #[test]
    fn every_refused_operation_would_have_been_held_back() {
        let mut recs = load();
        assert!(recs.len() > 800, "only {} operations recorded", recs.len());
        recs.sort_by(|a, b| (&a.timeline, a.start).cmp(&(&b.timeline, b.start)));
        let base = Instant::now();
        let at = |ms: i64, min: i64| base + Duration::from_millis((ms - min) as u64);
        let (mut refused, mut held_needlessly, mut violations) = (0, 0, Vec::new());
        let mut i = 0;
        while i < recs.len() {
            let tl = recs[i].timeline.clone();
            let min = recs[i].start;
            let mut budget = Budget::default();
            let mut shown: HashMap<(String, String), Shape> = HashMap::new();
            while i < recs.len() && recs[i].timeline == tl {
                let r = &recs[i];
                let key = (r.test.clone(), r.slot.clone());
                let (weight, shape) = match (&r.op[..], &r.content) {
                    ("send", Some(c)) => (send_weight(Shape::of(c)), Some(Shape::of(c))),
                    ("edit", Some(c)) => (edit_weight(shown.get(&key).copied(), Shape::of(c)), Some(Shape::of(c))),
                    _ => (Weight::Free, None),
                };
                let start = at(r.start, min);
                let would_send = budget.earliest(start, weight, Reserve::NONE) <= start;
                if r.refused {
                    refused += 1;
                    // The run after a bot restart that killed a probe mid-test (rate8-killed.txt):
                    // that process's last operations were never recorded.
                    let unrecorded = r.file == "rate10.txt" && r.test.starts_with("Y1 ") && r.n == 0;
                    if would_send && !unrecorded {
                        violations.push(format!("{} / {} #{} {} {} {:?}", r.file, r.test, r.n, r.slot, r.op, weight));
                    }
                } else {
                    held_needlessly += usize::from(!would_send);
                    budget.record(at(r.answered, min), weight);
                    match shape {
                        Some(s) => {
                            shown.insert(key, s);
                        }
                        None => {
                            shown.remove(&key);
                        }
                    }
                }
                i += 1;
            }
        }
        eprintln!(
            "{} operations, {refused} refused by Telegram, all held back by the bot; {held_needlessly} accepted ones the bot would have delayed",
            recs.len()
        );
        assert!(violations.is_empty(), "the bot would have sent these, and Telegram refused them:\n{}", violations.join("\n"));
    }
}
