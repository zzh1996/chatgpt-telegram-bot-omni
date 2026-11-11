#!/usr/bin/env python3
"""Model of Telegram's per-chat flood control for a bot's sends/edits/deletes, checked
against recorded experiments (reports of the bot's dev `edit_series` / `rate_tests`
probes, crates/omni-bot/src/devinject.rs). Findings: docs/research/telegram-flood.md.

telegram-rate-model.py <report.txt> [...]          replay every op with the model
telegram-rate-model.py --quiet <report.txt> [...]  only mismatches and totals
telegram-rate-model.py --scan <report.txt> [...]   per parameter: the values that fit as well
options: --set=name=value,...  override Params fields (e.g. --set=edit_t=8200,idle_reset=1e9)

The model (per bot and chat; size = code points of all text of the message):
  * HEAVY: a send of size >= SEND_T; an edit adding >= GROW_T to a message smaller than
    OLD_T; an edit of a message with 2..MAX_TOP top-level blocks where the size before
    and after is >= EDIT_T. Everything else is light. Deletes and refused ops are free.
  * Weights: light 1, heavy HEAVY_W.
  * Short window (sliding, SHORT_WINDOW s): at most SHORT_OPS ops, weight <= SHORT_W.
  * Long window (fixed, LONG_WINDOW s from the first accepted op after the previous one
    ran out): at most LONG_OPS ops, weight <= LONG_W. (IDLE_RESET, off by default: an
    early end after silence; measured: no such end after 20-36 s, see I20..I36, J36.)
  * Acceptance uses precise time. FLOOD_WAIT is computed in whole seconds: (second of
    the oldest op in the short window, or of the long window's start) + the window -
    (second now), at least MIN_WAIT.
Reports with wall-clock times are replayed as one timeline; older ones test by test.
"""
import functools
import math
import re
import sys
from dataclasses import dataclass, field, replace

UNITS = {  # body text of the probes (devinject.rs `script_content`): chars, UTF-8 bytes per unit
    "mixed": "这是一段用于测试编辑限速的文字，It is used to probe Telegram's edit rate limit. ",
    "ascii": "This text is used to probe Telegram's edit rate limit precisely. ",
    "cjk": "这是一段用于精确测量电报编辑频率限制的中文文字内容。",
    "ascii2": "Another sentence with other words: flood control measured here! ",
    "cjk2": "另外一句完全不同的话，专门拿来替换前面那段测试文本！",
    "mixed2": "换一段完全不同的内容再试，Completely other words replace the old body now. ",
    "emoji": "😀🎉🚀🌟",
}


@functools.cache
def text_blocks(script: str, chars: int, header: str, n: int) -> tuple[tuple[str, ...], ...]:
    """A probe message's top-level blocks, each as its text strings (devinject.rs
    `script_content`). The body is `chars` code points of a cycled text. Layouts
    `<layout><K>-<script>` (the body's last code point is the digit n % 10):
    plain (one paragraph), paraK (K even paragraphs), leadK (K paragraphs, the last
    K-1 of 100 each), detK (a details block titled "💭 Reasoning" with K paragraphs),
    hdetK (header, that details block, footer), hparaK (header, K paragraphs, footer).
    Any other script: a reply-like message (header, details with the body in
    paragraphs of 2000, footer)."""
    footer = f"edit {n} · {chars} chars"
    m = re.fullmatch(r"(plain|hpara|hdet|para|det|lead)(\d*)-(\w+)", script)
    if not m:
        b = body(script, chars)
        paras = tuple(b[i:i + 2000] for i in range(0, len(b), 2000))
        return (header,), ("💭 Reasoning", *paras), (footer,)
    layout, k, base = m[1], int(m[2] or 1), m[3]
    t = body(base, chars)[:-1] + str(n % 10) if chars else ""
    if layout == "lead":
        first = max(0, len(t) - 100 * (k - 1))
        paras = (t[:first], *(t[i:i + 100] for i in range(first, len(t), 100)))
    else:
        per = max(1, -(-len(t) // k))
        paras = tuple(t[i:i + per] for i in range(0, len(t), per)) or ("",)
    if layout in ("det", "hdet"):
        details = ("💭 Reasoning", *paras)
        return (details,) if layout == "det" else ((header,), details, (footer,))
    if layout == "hpara":
        return ((header,), *((x,) for x in paras), (footer,))
    return tuple((x,) for x in paras)


def body(script: str, chars: int) -> str:
    unit = UNITS.get(script, UNITS["mixed"])
    return (unit * (chars // len(unit) + 1))[:chars]


@dataclass
class Op:
    test: str
    n: int
    slot: str
    op: str  # send | edit | delete
    chars: int
    script: str
    t: float  # client start time, seconds since the test start
    wall: int | None  # client wall clock at the start, ms (newer reports)
    ok: bool
    wait: int | None  # FLOOD_WAIT seconds when refused
    raw: str = ""
    given: tuple[int, int] | None = None  # (code points, top-level blocks) of explicit-block ops

    @property
    def blocks(self) -> tuple[tuple[str, ...], ...]:
        if self.given is not None:  # stand-in blocks with the reported size and count
            cps, top = self.given
            return tuple(("x" * (cps // top + (i < cps % top)),) for i in range(top))
        if self.test.startswith("series "):
            header = f"🤖 edit-rate test · {self.test[7:]}"
        else:
            header = f"🤖 edit-rate test · {self.test} · {self.slot}"
        return text_blocks(self.script, self.chars, header, self.n)

    @property
    def cps(self) -> int:
        """Code points of all text."""
        return sum(len(x) for b in self.blocks for x in b)


@dataclass
class Params:
    # heaviness, from the size of the message (code points of all its text)
    send_t: int = 2110  # a send this big is heavy
    # a growing edit is heavy if it adds at least GROW_T and (ratio) the new text is at
    # least GROW_R times the old one, or (old) the old text is shorter than OLD_T
    grow_rule: str = "old"
    grow_t: int = 2110
    grow_r: float = 1.6
    old_t: int = 6000
    # an edit is heavy if the text before and after are both at least EDIT_T and the message
    # before the edit qualifies: (count) it has 2..MAX_TOP top-level blocks; (largest, refuted
    # by L1/L2) at least 2 top-level blocks, the largest at least BIG_T; (any) always
    edit_t: int = 8210
    struct_rule: str = "count"
    max_top: int = 3
    big_t: int = 2500
    # accounting
    heavy_w: int = 16  # weight of a heavy op (a light one weighs 1)
    short_window: float = 5.0
    short_ops: int = 5
    short_w: int = 20
    long_window: float = 60.0
    idle_reset: float = 1e9  # the long window also ends after this long without an accepted op...
    idle_light_only: bool = True  # ... but only while it holds light ops alone
    long_ops: int = 20
    long_w: int = 80
    min_wait: int = 3
    short_wait: str = "oldest"  # the 5 s wait runs until: the op whose leaving makes room (blocker) | the oldest op (oldest)
    clock: str = "seconds"  # seconds: waits from whole-second timestamps | exact: wait = ceil(time left)
    delta: float = 0.0  # server second boundaries vs the client wall clock (reports with wall times)


def parse(path: str) -> list[list[Op]]:
    """Tests (lists of ops in order) from a report."""
    tests: dict[str, list[Op]] = {}
    order: list[str] = []
    cur = None
    for line in open(path, encoding="utf-8"):
        line = line.rstrip("\n")
        if line.startswith("#"):
            continue
        m = re.match(r"^== (.*)$", line)
        if m:
            cur = m.group(1)
            tests[cur] = []
            order.append(cur)
            continue
        # rate_tests line
        m = re.match(
            r"^\s+#(?P<n>\d+)\s+(?P<slot>\S+)\s+(?P<op>send|edit|delete)\s+(?P<chars>\d+) chars\s+(?P<bytes>\d+) B"
            r"(?:\s+(?P<cps>\d+) cp)?(?:\s+(?P<top>\d+) top)? (?P<script>\S+)\s+at\s+[\d.]+s start\s+(?P<t>[\d.]+)s(?: wall (?P<wall>\d+))?"
            r" took\s+\d+ms\s+(?P<verdict>.*)$",
            line,
        )
        if m and cur:
            verdict = m["verdict"]
            if verdict.startswith("ERROR"):
                continue
            wait = None
            if verdict.startswith("FLOOD_WAIT") and verdict.split()[1] != "?":
                wait = int(verdict.split()[1])
            o = Op(cur, int(m["n"]), m["slot"], m["op"], int(m["chars"]), m["script"], float(m["t"]),
                   int(m["wall"]) if m["wall"] else None, verdict.startswith("ok"), wait, line)
            if m["script"] == "blocks":  # explicit blocks: size and top-level count as reported
                o.given = (int(m["cps"]) - int(m["top"] or 1), int(m["top"] or 1))  # cp column counts one separator per block
            tests[cur].append(o)
            continue
        # edit_series line
        m = re.match(r"^(\S+)\s+op\s+(\d+) size\s+(\d+)\s+t=\s*([\d.]+)s\s+took\s+(\d+)ms\s+(.*)$", line)
        if m:
            name = "series " + m.group(1)
            if name not in tests:
                tests[name] = []
                order.append(name)
            verdict = m.group(6)
            wait = int(re.search(r"FLOOD_WAIT (\d+)", verdict).group(1)) if "FLOOD" in verdict else None
            # op 0 of a series is the send; every later line edits the same message
            first = not any(o.ok and o.op == "send" for o in tests[name])
            tests[name].append(Op(name, int(m.group(2)), "A", "send" if first else "edit", int(m.group(3)), "mixed",
                                  float(m.group(4)), None, verdict == "ok", wait, line))
    return [tests[k] for k in order if tests[k]]


def size(blocks: tuple[tuple[str, ...], ...]) -> int:
    return sum(len(x) for b in blocks for x in b)


def structure_qualifies(p: Params, blocks: tuple[tuple[str, ...], ...]) -> bool:
    if p.struct_rule == "count":
        return 2 <= len(blocks) <= p.max_top
    if p.struct_rule == "largest":
        return len(blocks) >= 2 and max(size((b,)) for b in blocks) >= p.big_t
    return True


def is_heavy(p: Params, o: Op, old: tuple[str, tuple[tuple[str, ...], ...]] | None) -> bool:
    """`old`: (script, blocks) of the message before an edit."""
    new = size(o.blocks)
    if o.op == "send" or old is None:
        return new >= p.send_t
    before = size(old[1])
    if new - before >= p.grow_t and (new >= p.grow_r * before if p.grow_rule == "ratio" else before < p.old_t):
        return True
    return min(before, new) >= p.edit_t and structure_qualifies(p, old[1])  # the structure before the edit


@dataclass
class Verdict:
    ok: bool
    wait: int | None = None  # FLOOD_WAIT the model gives
    exact: float | None = None  # time until the op would fit
    edge: float = math.inf  # closeness of a deciding time to a window edge (acceptance)
    wait_edge: float = math.inf  # closeness of a deciding timestamp to a second boundary (wait value)
    why: str = ""


@dataclass
class Chat:
    p: Params
    phase: float = 0.0  # reports without wall times: server time = t + phase
    done: list[tuple[float, int, float]] = field(default_factory=list)  # accepted ops: (t, weight, server time)
    window: tuple[float, float] | None = None  # long window start: (t, server time)
    window_ops: int = 0
    window_w: int = 0
    last_ok: float = -math.inf  # time of the last accepted send/edit
    msgs: dict[str, tuple[str, tuple[tuple[str, ...], ...]]] = field(default_factory=dict)  # slot -> (script, blocks)

    def idle_ended(self, now: float, stretch: float = 0.0) -> bool:
        """The long window ended early: idle long enough (and, if so configured, light ops only)."""
        if now - self.last_ok < self.p.idle_reset + stretch:
            return False
        return not self.p.idle_light_only or self.window_w == self.window_ops

    def server(self, o: Op) -> float:
        return o.wall / 1000 + self.p.delta if o.wall is not None else o.t + self.phase

    def heavy(self, o: Op) -> bool:
        return o.op != "delete" and is_heavy(self.p, o, self.msgs.get(o.slot) if o.op == "edit" else None)

    def check(self, o: Op) -> Verdict:
        """Would Telegram accept `o` now? (No state change.) An edge case is a call
        that flips when the windows are EDGE longer or shorter (network jitter), or
        a wait that changes when a timestamp moves EDGE_S (second boundaries)."""
        v = self._check(o)
        if o.op != "delete":
            a, b = self._check(o, stretch=-EDGE), self._check(o, stretch=EDGE)
            v.edge = 0.0 if a.ok != b.ok else math.inf
            if not v.ok:
                waits = {self._check(o, jitter=(j1, j2)).wait for j1 in (-EDGE_S, EDGE_S) for j2 in (-EDGE_S, EDGE_S)}
                v.wait_edge = 0.0 if len(waits) > 1 else math.inf
        return v

    def _check(self, o: Op, stretch: float = 0.0, jitter: tuple[float, float] = (0.0, 0.0)) -> Verdict:
        p, now, srv = self.p, o.t, self.server(o) + jitter[1]
        if o.op == "delete":
            return Verdict(True)
        short_window, long_window = p.short_window + stretch, p.long_window + stretch
        w = p.heavy_w if self.heavy(o) else 1
        v = Verdict(True)
        waits = []  # (exact, reported, why)
        # short window: drop the oldest ops until the new one fits
        inside = sorted(x for x in self.done if x[0] > now - short_window)
        n, used = len(inside), sum(x[1] for x in inside)
        k = 0
        while k < n and (n - k + 1 > p.short_ops or used + w > p.short_w):
            used -= inside[k][1]
            k += 1
        if w > p.short_w:  # can never fit (only in parameter scans)
            waits.append((p.short_window, int(p.short_window), "short"))
        elif k:
            t, _, s = inside[k - 1] if p.short_wait == "blocker" else inside[0]
            waits.append((t + short_window - now, math.floor(s + jitter[0]) + int(p.short_window) - math.floor(srv), "short"))
        # long window
        if self.window is not None and now < self.window[0] + long_window and not self.idle_ended(now, stretch):
            if self.window_ops + 1 > p.long_ops or self.window_w + w > p.long_w:
                ws, wss = self.window
                waits.append((ws + long_window - now, math.floor(wss + jitter[0]) + int(p.long_window) - math.floor(srv), "long"))
        if waits:
            exact, rep, why = max(waits)
            v.ok, v.exact, v.why = False, exact, why
            v.wait = max(p.min_wait, rep if p.clock == "seconds" else math.ceil(exact - 1e-9))
        return v

    def apply(self, o: Op) -> None:
        """Telegram accepted `o`."""
        if o.op == "delete":
            self.msgs.pop(o.slot, None)
            return
        w = self.p.heavy_w if self.heavy(o) else 1
        if self.window is None or o.t >= self.window[0] + self.p.long_window or self.idle_ended(o.t):
            self.window, self.window_ops, self.window_w = (o.t, self.server(o)), 0, 0
        self.window_ops += 1
        self.window_w += w
        self.last_ok = o.t
        self.done.append((o.t, w, self.server(o)))
        self.msgs[o.slot] = (o.script, o.blocks)


EDGE = 0.06  # s: an op this close to a window edge may land on either side (network jitter)
EDGE_S = 0.03  # s: a timestamp this close to a second boundary may land in either second


@dataclass
class Score:
    ops: int = 0
    calls: int = 0  # wrong accept/refuse calls
    call_edges: int = 0  # ... of which at a window edge
    waits: int = 0  # wrong FLOOD_WAIT values (right calls)
    wait_edges: int = 0  # ... of which with a timestamp at a second boundary

    def bad(self) -> int:
        return self.calls - self.call_edges + self.waits - self.wait_edges


def timelines(tests: list[list[Op]]) -> list[list[Op]]:
    """Operations recorded with wall-clock times form one timeline across tests and reports
    (a test may start while the previous one's windows still run, e.g. after a pause cut
    short); older reports without wall times are replayed test by test (long pauses)."""
    walled = sorted((o for t in tests if t[0].wall is not None for o in t), key=lambda o: o.wall)
    others = [t for t in tests if t[0].wall is None]
    if not walled:
        return others
    w0 = walled[0].wall
    return others + [[replace(o, t=(o.wall - w0) / 1000) for o in walled]]


def replay(tests: list[list[Op]], p: Params, show: str = "", phases: dict[str, float] | None = None) -> Score:
    """Follows what really happened: only operations Telegram accepted change the state."""
    s = Score()
    for ops in tests:
        chat = Chat(p, phase=(phases or {}).get(ops[0].test, 0.0))
        label = None
        for o in ops:
            if show == "all" and o.test != label:
                label = o.test
                ph = f"  (server second phase {chat.phase:.2f})" if o.wall is None and p.clock == "seconds" else ""
                print(f"== {o.test}{ph}")
            v = chat.check(o)
            heavy = chat.heavy(o)
            if o.ok:
                chat.apply(o)
            s.ops += 1
            call_ok = v.ok == o.ok
            wait_ok = o.ok or not call_ok or o.wait is None or v.wait == o.wait  # None: not recorded
            edge = (not call_ok and v.edge < EDGE) or (not wait_ok and v.wait_edge < EDGE_S)
            s.calls += not call_ok
            s.call_edges += not call_ok and v.edge < EDGE
            s.waits += not wait_ok
            s.wait_edges += not wait_ok and v.wait_edge < EDGE_S
            bad = not call_ok or not wait_ok
            if show == "all" or (show == "bad" and bad):
                mark = "  " if not bad else ("e " if edge else "!!")
                real = "ok" if o.ok else f"FLOOD {o.wait}"
                model = "ok" if v.ok else f"FLOOD {v.wait} ({v.why} {v.exact:.2f}s)"
                size = f"{o.cps:6} cp {o.script:11}" if o.op != "delete" else " " * 21
                where = "" if show == "all" else f"{o.test[:30]:30} "
                kind = "-" if o.op == "delete" else ("H" if heavy else "L")
                print(f"{mark} {where}{o.slot:2} {o.op:6} {size} {kind} t={o.t:7.3f}  real {real:9} model {model}")
    return s


def fit_clock(tests: list[list[Op]], p: Params) -> tuple[Params, dict[str, float]]:
    """Where the server's second boundaries fall. Reports with wall times share one
    offset (`delta`: the server clock vs ours, plus latency); for the others each test
    gets its own (where its start fell within a server second is unknown)."""
    if p.clock != "seconds":
        return p, {}

    def key(sc: Score) -> tuple[int, int]:
        return sc.bad(), sc.call_edges + sc.wait_edges

    walled = [t for t in tests if t[0].wall is not None]
    if walled:
        p = replace(p, delta=min((key(replay(walled, replace(p, delta=d / 100))), d / 100) for d in range(100))[1])
    phases = {}
    for t in tests:
        if t[0].wall is None:
            phases[t[0].test] = min((key(replay([t], p, phases={t[0].test: f / 100})), f / 100) for f in range(100))[1]
    return p, phases


SCAN = {
    "send_t": range(1000, 3001, 1),
    "edit_t": range(7500, 9001, 1),
    "grow_r": [x / 1000 for x in range(1000, 3001)],
    "old_t": range(1000, 12001, 1),
    "max_top": range(1, 12),
    "big_t": range(100, 9001, 1),
    "grow_t": range(1000, 6001, 1),
    "heavy_w": range(2, 21),
    "short_ops": range(3, 9),
    "short_w": range(4, 41),
    "long_ops": range(15, 26),
    "long_w": range(20, 101),
    "idle_reset": [x / 2 for x in range(10, 141)] + [1e9],
}


def scan(tests: list[list[Op]], p: Params) -> None:
    """Each parameter on its own (the others at their values): which values make no more
    wrong accept/refuse calls than the current ones (window-edge calls and waits ignored)."""
    base = (lambda sc: sc.calls - sc.call_edges)(replay(tests, p))
    print(f"wrong calls (not at a window edge) with the current values: {base}")
    unused = {"ratio": {"old_t"}, "old": {"grow_r"}}[p.grow_rule] | {"count": {"big_t"}, "largest": {"max_top"}, "any": {"big_t", "max_top"}}[p.struct_rule]
    for name, rng in SCAN.items():
        if name in unused:
            continue
        values = list(rng)
        fit = [(lambda sc: sc.calls - sc.call_edges <= base)(replay(tests, replace(p, **{name: v}))) for v in values]
        runs, i = [], 0
        while i < len(values):
            if fit[i]:
                j = i
                while j + 1 < len(values) and fit[j + 1]:
                    j += 1
                runs.append(f"{values[i]}..{values[j]}" if j > i else f"{values[i]}")
                i = j + 1
            else:
                i += 1
        print(f"{name:10} now {getattr(p, name)!s:6}  fits: {', '.join(runs) or 'nothing'}  (tried {values[0]}..{values[-1]})")


def main(argv: list[str]) -> None:
    paths = [a for a in argv if not a.startswith("--")]
    tests = timelines([t for path in paths for t in parse(path)])
    p = Params()
    for a in argv:
        if a.startswith("--set="):
            for kv in a[6:].split(","):
                k, val = kv.split("=")
                p = replace(p, **{k: type(getattr(p, k))(val)})
    if "--scan" in argv:
        scan(tests, p)
        return
    p, phases = fit_clock(tests, p)
    s = replay(tests, p, show="bad" if "--quiet" in argv else "all", phases=phases)
    print(f"\n{s.ops} ops: {s.calls} wrong accept/refuse calls ({s.call_edges} at a window edge), "
          f"{s.waits} wrong FLOOD_WAIT values ({s.wait_edges} at a second boundary)\n{p}")


if __name__ == "__main__":
    main(sys.argv[1:])
