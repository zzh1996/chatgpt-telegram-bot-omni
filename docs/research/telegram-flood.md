# Telegram flood control for a bot's messages in one chat (measured 2026-11-11)

Telegram documents only "about 20 messages per minute in a group" for bots. Everything below was measured on
2026-11-11 with the bot's dev probes (`edit_series` / `rate_tests` in `crates/omni-bot/src/devinject.rs`) in the
test supergroup, with an MTProto bot account (grammers, layer 233) sending, editing and deleting **rich messages**.
The raw reports are in [`telegram-flood/`](telegram-flood/). `tools/telegram-rate-model.py` implements the model below
and replays every recorded operation against it:

```
python3 tools/telegram-rate-model.py --quiet docs/research/telegram-flood/*.txt   # mismatches only
python3 tools/telegram-rate-model.py         docs/research/telegram-flood/*.txt   # every op: real vs model
python3 tools/telegram-rate-model.py --scan  docs/research/telegram-flood/*.txt   # which parameter values fit
```

## 1. The model

Per bot and chat. **Size** = Unicode code points of all text in the message (all blocks of a rich message: the
unit of Telegram's 32768 rich-message limit). Bytes and UTF-16 units do not matter, nor does the content: an edit
that rewrites everything with different text of the same size is classified like an edit of one character.

**Heavy or light.** An operation is *heavy* if

1. it **sends** a message of size ≥ **S** ≈ 2110 (2100 light, 2120 heavy); or
2. it **edits** a message, adding ≥ **G** ≈ 2110 code points (2100 light, 2130 heavy) to a message that had
   < **O** ≈ 6000 (5000 heavy, 7000 light); or
3. it **edits** a big message (old and new size both ≥ **E** ≈ 8210; 8194 light, 8225 heavy) with certain
   layouts. Which layouts is **not understood**: the block count alone does not explain it (rate19, below).
   Heavy: 2 or 3 paragraphs; the reply layout (header + details + footer); header + details + divider + 2
   footers; header + details + 3 footers; a paragraph + a details block; two details blocks; 5 paragraphs at
   30000. Light up to 20000: 1 paragraph; 4, 5, 7 or 9 paragraphs; a details block alone; a details block (model
   name in its title) + 1 footer. **The bot counts every edit with both sizes ≥ 8000 as heavy** until the rule is
   known: long replies update more slowly, never into a `FLOOD_WAIT`.

Everything else is *light*: small edits, shrinking edits, deletes.

**What counts.** Sends and edits that Telegram accepted. Refused operations and deletes cost nothing. A light
operation weighs **1**, a heavy one **16**.

**Short window**: any **5 seconds** (sliding, decided with millisecond precision) may hold at most **5 operations**,
of which at most **1 heavy** (one heavy plus four light is fine, two heavy never).

**Long window**: **60 seconds, fixed**. It starts with the first accepted operation after the previous window ran
out (not sliding, not aligned to the clock) and may hold at most **20 operations** and weight **≤ 80**:

| heavy ops in the window | light ops that still fit |
|---|---|
| 0 | 20 |
| 1 | 19 |
| 2 | 18 |
| 3 | 17 |
| 4 | 16 |
| 5 | none: a 5th heavy op fits only if the window holds no light op |

So in the long run a chat takes 20 light operations per minute, or 5 heavy ones, or any mix that fits both limits.

**The wait.** A refused operation gets `FLOOD_WAIT_X` computed from **whole-second** timestamps: for the short window,
X = (second of the oldest operation in the window) + 5 − (current second); for the long window, X = (second the window
started) + 60 − (current second); the larger if both; and **at least 3**. So X can be up to 1 s shorter than the real
remaining time (retry after X + 1 s), and `FLOOD_WAIT_3` often means "less than 3 s".

**Window boundaries in practice.** Because the long window is fixed and starts at whatever operation comes first,
two bursts 50–60 s apart often share a window: the earlier burst's last operations open a window that the next
burst starts in. A window does not end early when the chat goes quiet (20–36 s of silence measured).

## 2. How it was measured

- **Probes.** Each `rate_tests` step sends, edits or deletes a probe message at an exact time (a private gate with
  no spacing; on FLOOD_WAIT it waits and, for setup sends, retries). Probe texts are a cycled sentence (mixed
  Chinese/English, ASCII, CJK or emoji, or a second sentence with no common text) in several layouts: a reply-like
  message (header paragraph, details block "💭 Reasoning" with the body in 2000-code-point paragraphs, footer), one
  paragraph, K even paragraphs, one big paragraph plus small ones, the paragraphs inside a details block, and so on.
  Every version differs from the previous one (a footer counter or the last digit); from `rate10` on, every edit
  is also checked for `MESSAGE_NOT_MODIFIED` and every message is read back at the end of a test.
- **Heavy or light?** Op X at t, then a known-heavy probe (editing a 20000-code-point reply) at t + 1 s: refused with
  `FLOOD_WAIT_4` means X was heavy (two heavy ops in 5 s), accepted means light. A test sends the 20000 probe, then
  X's message at 6 s (classifies the send), edits it at 13 s (classifies the edit), with the probe at 7 s and 14 s.
- **Budgets.** Series of ops at fixed spacing until refused (20 light per minute; 5 heavy per minute; 4 heavy then 16
  light; 5 heavy then not even 1 light; 1 heavy + 4 light within 3 s; …), deletes inside a full window, and probe
  pairs placed at chosen positions within wall-clock seconds for the window edges and the rounding of the wait.

## 3. Evidence

Test names refer to the reports in [`telegram-flood/`](telegram-flood/). Sizes are code points of the whole message.

**Sends** (the probe after the send tells heavy from light):

| size | layout | result | tests |
|---|---|---|---|
| 569, 1101 (emoji: 2200 UTF-16 units, 4.4 KB), 1483 (CJK, 4.3 KB), 2048, 2100 | various | light | T2, S2, F1, S1 |
| 2120, 2140, 2155, 2157, 2160–2199 | one paragraph | heavy | S7, S6, C1, C2 |
| 2483 (CJK), 3085, 4083 (ASCII, 4.1 KB), 7083, 9000–20000 | various | heavy | F1–F3, S3–S5, all classify tests |

So sends count code points, not bytes (1483 CJK = 4.3 KB light, 4083 ASCII = 4.1 KB heavy) nor UTF-16 units (1100
emoji light).

**Edits that grow the text** (old → new size):

| old → new | added | result | tests |
|---|---|---|---|
| 152 → 2153, …, 8153 → 10154 (2000 at a time) | +2000 | light | grow series |
| 1001 → 2901, 301 → 2401 | +1900, +2100 | light | Y6, P1 |
| 301 → 2431, 2451, 2501, 2601, 3001; 501 → 3001 (CJK), 4596, 5501 (ASCII) | +2130 … +5000 | heavy | G3, G4, P2, P3, Y2, K5, G1/G2, K6 |
| 3001 → 5801 / 6101, 3501 → 5901, 4001 → 6201, 4501 → 9001, 5001 → 7501 / 9801 | +2200 … +4800 | heavy | Y3, Y4, Z1–Z3, R1, R2 |
| 7001 → 9501, 7901 → 10201, 8001 → 12501, 8178 → 12179 | +2300 … +4500 | light | O1, O2, Y1, E6 |

A ratio rule does not fit (×1.50 and ×1.55 heavy from 5000/4000, ×1.93 light from 2153); neither does an absolute one
(+4500 light from 8001). Added CJK text counts by code points too (+2500 CJK = 7.5 KB heavy, +5000 ASCII heavy).

**Edits of a big message**: same size before and after, changing the last digit or the footer (or everything):

| top-level blocks before the edit | messages (size) | result |
|---|---|---|
| 1 | one paragraph 8191 … 10000 (M1–M9, N1); a details block of 5 paragraphs, 9000 (N4) | light |
| 2 | 2 paragraphs of 9000, 8330, 8250, 8225 (A1, N2, T2, T1, E9) | heavy |
| 3 | 3 paragraphs, 9000 (A2); header + details + footer: reply layout 8377 … 30000 (E7, C3–C6, K1, series), 5 paragraphs in the details (N5), 1 (N8) | heavy |
| 3 | reply layout 8194 (E5), 5000 (5K series, C7/C8) | light (below E) |
| 4 | 4 paragraphs, 9000 (L3) | light |
| 5 | 5 paragraphs, 9000 and 10000 (A3, N3, N7); one 8600 + four 100 (L1) | light |
| 7 | header + 5 paragraphs + footer, 9000 (N6) | light |
| 9 | 9 paragraphs, 8250 … 9000 (A4, B1–B3); one 8200 + eight 100 (L2) | light |
| 1, 5, 9 | 20000 (U1–U3) | light |
| 3 → 5 / 5 → 3 | 9000, re-split by the edit (X1 / X2) | heavy / light |

**rate19** (reasoning-phase layouts, ~9050 code points of reasoning; probe: a heavy edit 1 s after the edit under
test is refused iff that edit was heavy):

| layout (top-level blocks) | result |
|---|---|
| L-A: header paragraph, details, divider, 2 footers (5) | heavy |
| L-B: details titled "🤖 model · 💭 Reasoning", 1 footer (2) | light |
| L-C: small-text header, details, divider, 2 footers (5) | heavy |
| L-D: header paragraph, details, 3 footers (5) | heavy |
| R1: paragraph (30), details (2) | heavy |
| R2: two details blocks (2) | heavy |

So neither "2-3 blocks" nor "details blocks do not count" holds; L-B is the only reply-like layout that stayed light.

Rewriting all text: K1 (9095, mixed → other sentence) heavy, E4/E5 (5000 CJK, 8194) light: content does not matter.
Shrinking: 20000 → 500 light (C1); growing past E from a small text is the growth rule (500 → 20000 heavy, C2).

**Windows and weights:**

| what | tests |
|---|---|
| 20 light per minute; a delete inside a full minute is free | T3, D1 |
| 5 heavy per minute, the 6th waits for the minute's end | 10K/20K/30K series, T1, T4, Q1 |
| 4 heavy + 4 light, then the 5th heavy refused | T2 |
| 6 light + 4 heavy, then the 5th heavy refused (wait to the minute that began with the first light op) | grow series |
| 4 heavy + 8 light accepted; 4 heavy + 16 light accepted, the 17th light refused (21st op) | G3, LA |
| 5 heavy, then even one light refused | G2 |
| a new minute starts with the first accepted op after the previous one ran out | T1–T4, 10K series, W2 |
| a light op 20, 25 or 30 s before 5 heavy ops still anchors their minute; 17 light ops, 36 s of silence, then the 21st op of the minute is refused | I20, I25, I30, J36 |
| two heavy ops within 5 s: always refused (3.2 … 4.95 s apart) | everywhere, Q2, Q3 |
| 1 heavy + 4 light within 3.2 s, in either order: accepted; a 6th op in 5 s refused | G1, SA |
| 5 light per 5 s (sliding), the 6th refused | T3 |
| decided with ms precision: 4.55 s and 4.95 s refused, 5.10 s accepted | Q2, Q3 |
| waits in whole seconds: probe pairs at chosen positions within a second gave 4, 5, 5, 34 | Q1 |
| refused operations cost nothing | T1, T3 (refused ops followed by accepted ones) |

**Fit.** All but four of the 808 recorded operations (113 tests) replay with the right call and, for refusals, the exact
`FLOOD_WAIT` value; reports with wall-clock times are replayed as one timeline (a test may start while the previous
one's windows still run) with one clock offset (δ = 0.04 s, our clock plus latency against the server's seconds);
older reports get one offset each. The scan shows the ranges above (`--scan`).

## 4. What this means for the bot's replies

- **Reasoning phase.** A reply that is still reasoning is header + details + footer: 3 top-level blocks. Once its
  text reaches ~8.2K code points every edit is heavy: at most one per 5 s and 5 per minute (12 s on average), less
  if anything else happens in the chat. This is what stalled the DeepSeek reply (FLOOD_WAIT 3 every other edit at a
  3 s pace, then ~25 s waits).
- **Answer phase.** Some layouts with many blocks stay light, others do not (rate19); the bot treats every edit of
  a reply ≥ 8000 code points as heavy, so long replies update about every 12 s.
- **Big jumps.** An edit that adds ≥ ~2.1K code points to a message under ~6K is heavy: e.g. the first update after
  a long wait, or a fast model between two updates. A message sent with ≥ ~2.1K code points is heavy.
- **The old pacing.** 3 s between operations is exactly 20 per minute: fine for light operations, too fast for heavy
  ones (two within 5 s are refused). Pacing has to know the weight of each operation.

## 5. Not covered

Private chats (not measured; the bot paces them with this model too, as they are at most as strict), channels, other chats of the same
bot running at the same time, classic (non-rich) text messages, media and captions, block types other than
paragraphs, details, dividers and footers (lists, code, tables, quotes), what decides the big-edit rule (§1, rule 3),
edits that change an early block rather than the last ones, sizes above 30000 code points.

## 6. The four operations the model does not reproduce

- **T3 #8**: an edit exactly 5.000 s after the oldest of the five operations before it (a 1 s cadence). The same spacing was refused two
  operations later (#10); the side of the window edge is decided by milliseconds of network jitter.
- **I36 #3**: a heavy edit 5.097 s after the previous heavy one (by our clock) was refused; that previous edit took
  437 ms instead of the usual ~230 ms, so it most likely reached the server late and the gap there was under 5 s.
- **Q1 #8**: refused as the model says, but with `FLOOD_WAIT_34` (a full minute window) instead of 5: the server's
  minute had started with Q1's first operation, while the model starts it 37 s earlier at the single light operation
  that ended D1 (accepted right after D1's minute ran out; its message was deleted immediately after). Windows that
  hold light operations do not end after 20–36 s of silence (I20…I30, J36), and deleting a message does not refund its
  operations (D1); what made this window end early is unknown.
- **Y1, second attempt**: a probe file was started by an older bot process that the restart killed after three
  operations (reconstructed from the outbox in `rate8-killed.txt`); with those, the first refusal of the next run is
  reproduced, the second (3 s after the minute ran out) is not.
