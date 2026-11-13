//! LaTeX for Telegram's formula renderers.
//!
//! Each Telegram app renders a rich message's formulas with its own engine
//! (source read 2026-11-14; tested with the owner on every app):
//! - Android: JLaTeXMath 1.0.7 (Telegram's fork). A regression in it (the fork's
//!   commit f59d6ee) breaks every environment and the macros JLaTeXMath defines
//!   as "new commands": `\dfrac`, `\tfrac`, `\dbinom`, `\operatorname`,
//!   `\textbf`, `\textit`, `\textrm`, `\textsf`, `\texttt`, `\pmod`, `\mod`,
//!   `\substack`. A block formula that fails is not shown at all.
//! - iOS and macOS: SwiftMath. No `\boxed`, `\fbox`, `\underbrace`, `\overset`,
//!   `\xrightarrow`, `\middle`, `\mbox`, `array`, `align`, `gathered`, `\dots`,
//!   `\bmod`, `\lvert`, `\tag`, `\:` …; a formula that fails is shown as its
//!   source. `\\` outside an environment makes rows on iOS but merges into one
//!   line on macOS. A prime `'` and characters beyond ASCII (other than Cyrillic
//!   and accented Latin letters) are dropped silently outside `\text{}`.
//! - Desktop: MicroTeX (a JLaTeXMath port): nearly everything.
//!
//! [`for_telegram`] (inline) and [`display_lines`] (a block formula) rewrite
//! what models write most into what all of them render, keeping the meaning:
//! fraction and binomial styles become `\frac`/`\binom`, `\operatorname{f}`
//! `\mathrm{f}`, `\pmod{p}` (or `\pmod p`: an argument may be one token, as TeX
//! allows) `\quad(\mathrm{mod}\,p)`, a box or a colour leaves its content,
//! `\textbf` and friends `\text`, `f'` `f^{\prime}`, symbols such as `×` and `≤`
//! their commands, other text beyond ASCII goes into `\text{}`, symbols iOS and
//! macOS lack (`\therefore`) their characters in `\text{}`, and small spellings
//! get their common forms. A block formula of several lines (alignment
//! environments, `\\`, a system in `\left\{ … \right.`, the rows of `cases`)
//! becomes one formula per line, without `&` and equation tags; one that is
//! only an `array` (a table of values) becomes a table ([`array_table`]); any
//! other `array` a `matrix`. Anything else is sent as written: matrices stay
//! (Android shows none), and so do `\underbrace`, `\overset` and `\xrightarrow`
//! (iOS and macOS show their source).

/// Alignment environments: their lines stay, the alignment goes.
const ALIGN_ENVS: &[&str] = &[
    "aligned",
    "aligned*",
    "align",
    "align*",
    "gather",
    "gather*",
    "gathered",
    "split",
    "alignat",
    "alignat*",
    "alignedat",
    "eqnarray",
    "eqnarray*",
    "multline",
    "multline*",
    "flalign",
    "flalign*",
    "equation",
    "equation*",
    "displaylines",
];

/// Environments whose rows are cases: `value & condition`.
const CASES_ENVS: &[&str] = &["cases", "dcases", "rcases", "cases*"];

/// Commands replaced by others (same arguments).
const RENAMED: &[(&str, &str)] = &[
    ("dfrac", "frac"),
    ("tfrac", "frac"),
    ("dbinom", "binom"),
    ("tbinom", "binom"),
    ("operatorname", "mathrm"),
    ("textbf", "text"),
    ("textit", "text"),
    ("textrm", "text"),
    ("textnormal", "text"),
    ("textsf", "text"),
    ("texttt", "text"),
    ("textup", "text"),
    ("textsc", "text"),
    ("emph", "text"),
    ("mbox", "text"),
    ("hbox", "text"),
    ("mathscr", "mathcal"),
    ("bm", "boldsymbol"),
    ("pmb", "boldsymbol"),
];

/// Commands written another way (no arguments involved).
const RESPELLED: &[(&str, &str)] = &[
    ("dots", "\\ldots"),
    ("dotsc", "\\ldots"),
    ("dotso", "\\ldots"),
    ("dotsb", "\\cdots"),
    ("dotsm", "\\cdots"),
    ("dotsi", "\\cdots"),
    ("lvert", "|"),
    ("rvert", "|"),
    ("lVert", "\\|"),
    ("rVert", "\\|"),
    // Spaced like amsmath's (a bare `\mathrm{mod}` would touch its neighbours).
    ("bmod", "\\;\\mathrm{mod}\\;"),
    ("mod", "\\quad\\mathrm{mod}\\;"),
    ("lcm", "\\mathrm{lcm}"),
    ("impliedby", "\\;\\Longleftarrow\\;"),
    ("degree", "^{\\circ}"),
    ("coloneqq", ":="),
    ("eqqcolon", "=:"),
    ("enspace", "\\;"),
    ("hfill", "\\quad "),
    ("newline", "\\\\"),
    ("hline", ""),
    ("nonumber", ""),
    ("notag", ""),
    ("dag", "\\dagger"),
    ("ddag", "\\ddagger"),
    ("vartriangle", "\\triangle"),
    ("ldotp", "."),
    // Symbols iOS and macOS lack, as the characters (drawn in `\text{}` everywhere).
    ("therefore", "\\text{∴}"),
    ("because", "\\text{∵}"),
    ("blacksquare", "\\text{■}"),
    ("checkmark", "\\text{✓}"),
    ("complement", "\\text{∁}"),
    ("triangleq", "\\text{≜}"),
    ("rightleftharpoons", "\\text{⇌}"),
    ("leftrightharpoons", "\\text{⇋}"),
    ("lesssim", "\\text{≲}"),
    ("gtrsim", "\\text{≳}"),
    ("approxeq", "\\text{≊}"),
    ("S", "\\text{§}"),
    ("P", "\\text{¶}"),
];

/// Commands that leave only their (last) argument, after skipping this many.
const UNWRAPPED: &[(&str, usize)] =
    &[("boxed", 0), ("fbox", 0), ("framebox", 0), ("color", 1), ("textcolor", 1), ("colorbox", 1)];

/// Commands replaced with their one argument: equation numbers, labels, table
/// rules and invisible boxes go, spacing becomes `\quad`.
const DROPPED: &[(&str, &str)] = &[
    ("tag", ""),
    ("tag*", ""),
    ("label", ""),
    ("cline", ""),
    ("phantom", ""),
    ("hphantom", ""),
    ("vphantom", ""),
    ("vspace", ""),
    ("vspace*", ""),
    ("hspace", "\\quad "),
    ("hspace*", "\\quad "),
];

/// Characters beyond ASCII and their commands (every app renders these).
const SYMBOLS: &[(char, &str)] = &[
    ('×', "\\times"),
    ('÷', "\\div"),
    ('±', "\\pm"),
    ('∓', "\\mp"),
    ('·', "\\cdot"),
    ('⋅', "\\cdot"),
    ('∙', "\\cdot"),
    ('−', "-"),
    ('–', "-"),
    ('∗', "\\ast"),
    ('∘', "\\circ"),
    ('⊕', "\\oplus"),
    ('⊗', "\\otimes"),
    ('∧', "\\wedge"),
    ('∨', "\\vee"),
    ('¬', "\\neg"),
    ('∪', "\\cup"),
    ('∩', "\\cap"),
    ('∖', "\\setminus"),
    ('≤', "\\le"),
    ('≥', "\\ge"),
    ('⩽', "\\le"),
    ('⩾', "\\ge"),
    ('≠', "\\ne"),
    ('≈', "\\approx"),
    ('≡', "\\equiv"),
    ('∼', "\\sim"),
    ('≃', "\\simeq"),
    ('≅', "\\cong"),
    ('∝', "\\propto"),
    ('≪', "\\ll"),
    ('≫', "\\gg"),
    ('∈', "\\in"),
    ('∉', "\\notin"),
    ('∋', "\\ni"),
    ('⊂', "\\subset"),
    ('⊃', "\\supset"),
    ('⊆', "\\subseteq"),
    ('⊇', "\\supseteq"),
    ('⊊', "\\subsetneq"),
    ('⊥', "\\perp"),
    ('∥', "\\parallel"),
    ('∣', "\\mid"),
    ('→', "\\to"),
    ('←', "\\leftarrow"),
    ('↔', "\\leftrightarrow"),
    ('⇒', "\\Rightarrow"),
    ('⇐', "\\Leftarrow"),
    ('⇔', "\\Leftrightarrow"),
    ('⟹', "\\Longrightarrow"),
    ('⟸', "\\Longleftarrow"),
    ('⟺', "\\Longleftrightarrow"),
    ('⟶', "\\longrightarrow"),
    ('↦', "\\mapsto"),
    ('↑', "\\uparrow"),
    ('↓', "\\downarrow"),
    ('∑', "\\sum"),
    ('∏', "\\prod"),
    ('∫', "\\int"),
    ('∬', "\\iint"),
    ('∮', "\\oint"),
    ('∂', "\\partial"),
    ('∇', "\\nabla"),
    ('∞', "\\infty"),
    ('∅', "\\emptyset"),
    ('∀', "\\forall"),
    ('∃', "\\exists"),
    ('∄', "\\nexists"),
    ('∠', "\\angle"),
    ('△', "\\triangle"),
    ('°', "^{\\circ}"),
    ('…', "\\ldots"),
    ('⋯', "\\cdots"),
    ('⋮', "\\vdots"),
    ('⋱', "\\ddots"),
    ('⌊', "\\lfloor"),
    ('⌋', "\\rfloor"),
    ('⌈', "\\lceil"),
    ('⌉', "\\rceil"),
    ('⟨', "\\langle"),
    ('⟩', "\\rangle"),
    ('ℝ', "\\mathbb{R}"),
    ('ℕ', "\\mathbb{N}"),
    ('ℤ', "\\mathbb{Z}"),
    ('ℚ', "\\mathbb{Q}"),
    ('ℂ', "\\mathbb{C}"),
    ('ℏ', "\\hbar"),
    ('ℓ', "\\ell"),
    ('α', "\\alpha"),
    ('β', "\\beta"),
    ('γ', "\\gamma"),
    ('δ', "\\delta"),
    ('ε', "\\varepsilon"),
    ('ϵ', "\\epsilon"),
    ('ζ', "\\zeta"),
    ('η', "\\eta"),
    ('θ', "\\theta"),
    ('ϑ', "\\vartheta"),
    ('ι', "\\iota"),
    ('κ', "\\kappa"),
    ('λ', "\\lambda"),
    ('μ', "\\mu"),
    ('ν', "\\nu"),
    ('ξ', "\\xi"),
    ('ο', "o"),
    ('π', "\\pi"),
    ('ϖ', "\\varpi"),
    ('ρ', "\\rho"),
    ('ϱ', "\\varrho"),
    ('σ', "\\sigma"),
    ('ς', "\\varsigma"),
    ('τ', "\\tau"),
    ('υ', "\\upsilon"),
    ('φ', "\\varphi"),
    ('ϕ', "\\phi"),
    ('χ', "\\chi"),
    ('ψ', "\\psi"),
    ('ω', "\\omega"),
    ('Α', "A"),
    ('Β', "B"),
    ('Γ', "\\Gamma"),
    ('Δ', "\\Delta"),
    ('∆', "\\Delta"),
    ('Ε', "E"),
    ('Ζ', "Z"),
    ('Η', "H"),
    ('Θ', "\\Theta"),
    ('Ι', "I"),
    ('Κ', "K"),
    ('Λ', "\\Lambda"),
    ('Μ', "M"),
    ('Ν', "N"),
    ('Ξ', "\\Xi"),
    ('Ο', "O"),
    ('Π', "\\Pi"),
    ('Ρ', "P"),
    ('Σ', "\\Sigma"),
    ('Τ', "T"),
    ('Υ', "\\Upsilon"),
    ('Φ', "\\Phi"),
    ('Χ', "X"),
    ('Ψ', "\\Psi"),
    ('Ω', "\\Omega"),
    ('\u{2126}', "\\Omega"),
];

/// Superscript and subscript characters: `^` or `_`, and what they raise or lower.
fn script_char(c: char) -> Option<(char, char)> {
    const SUP: &[(char, char)] = &[
        ('⁰', '0'),
        ('¹', '1'),
        ('²', '2'),
        ('³', '3'),
        ('⁴', '4'),
        ('⁵', '5'),
        ('⁶', '6'),
        ('⁷', '7'),
        ('⁸', '8'),
        ('⁹', '9'),
        ('⁺', '+'),
        ('⁻', '-'),
        ('⁼', '='),
        ('⁽', '('),
        ('⁾', ')'),
        ('ⁿ', 'n'),
        ('ⁱ', 'i'),
    ];
    const SUB: &[(char, char)] = &[
        ('₀', '0'),
        ('₁', '1'),
        ('₂', '2'),
        ('₃', '3'),
        ('₄', '4'),
        ('₅', '5'),
        ('₆', '6'),
        ('₇', '7'),
        ('₈', '8'),
        ('₉', '9'),
        ('₊', '+'),
        ('₋', '-'),
        ('₌', '='),
        ('₍', '('),
        ('₎', ')'),
        ('ₐ', 'a'),
        ('ₑ', 'e'),
        ('ₒ', 'o'),
        ('ₓ', 'x'),
        ('ₙ', 'n'),
        ('ᵢ', 'i'),
        ('ⱼ', 'j'),
    ];
    let find = |t: &[(char, char)]| t.iter().find(|(u, _)| *u == c).map(|&(_, a)| a);
    find(SUP).map(|a| ('^', a)).or_else(|| find(SUB).map(|a| ('_', a)))
}

/// How many primes a character is.
fn primes(c: char) -> usize {
    match c {
        '\'' | '′' => 1,
        '″' => 2,
        '‴' => 3,
        _ => 0,
    }
}

/// Characters beyond ASCII that every app draws in a formula: accented Latin
/// and Cyrillic letters.
fn drawn_as_is(c: char) -> bool {
    matches!(c, '\u{C0}'..='\u{FF}' | '\u{410}'..='\u{44F}')
}

/// Text beyond ASCII that goes into `\text{}` (CJK, emoji, other symbols).
fn wants_text(c: char) -> bool {
    !c.is_ascii()
        && !c.is_whitespace()
        && !matches!(c, '\u{200B}' | '\u{2060}' | '\u{FEFF}')
        && !drawn_as_is(c)
        && primes(c) == 0
        && script_char(c).is_none()
        && !SYMBOLS.iter().any(|(u, _)| *u == c)
}

/// A block formula that is only an `array`: a table of values, as models write
/// tables in formulas. Android shows no environment and iOS and macOS no
/// `array`, so it becomes a table with a formula in each cell.
#[derive(Debug, PartialEq)]
pub struct ArrayTable {
    /// Each column's alignment: `l`, `c` or `r`.
    pub align: Vec<char>,
    /// A rule under the first row makes it the header.
    pub header: bool,
    /// The cells, as inline formulas ([`for_telegram`]; empty when empty).
    pub rows: Vec<Vec<String>>,
}

/// The table of a block formula that is only an `array` (see [`ArrayTable`]).
pub fn array_table(tex: &str) -> Option<ArrayTable> {
    let chars: Vec<char> = tex.trim().chars().collect();
    let open: Vec<char> = "\\begin{array}".chars().collect();
    if !chars.starts_with(&open) {
        return None;
    }
    let (env, body_start, body_end, end) = environment(&chars, "\\begin".len())?;
    if env != "array" || end != chars.len() {
        return None;
    }
    // The column spec without its arguments (`p{2cm}`, `@{}`): `l`, `c`, `r` are the columns.
    let k = skip_spaces(&chars, open.len());
    let mut align = Vec::new();
    if chars.get(k) == Some(&'{') {
        let e = group_end(&chars, k)?;
        let mut j = k + 1;
        while j < e - 1 {
            match chars[j] {
                '{' => j = group_end(&chars, j)?,
                c @ ('l' | 'c' | 'r') => {
                    align.push(c);
                    j += 1;
                }
                'p' | 'm' | 'b' => {
                    align.push('l');
                    j += 1;
                }
                _ => j += 1,
            }
        }
    }
    let body: String = chars[body_start..body_end].iter().collect();
    let mut header = false;
    let mut rows = Vec::new();
    for line in lines_of(&body) {
        let mut row = line.as_str();
        // `\hline` (and `\cline{…}`) rules: one before the second row makes a header.
        loop {
            let r = row.trim_start();
            if let Some(rest) = r.strip_prefix("\\hline") {
                header |= rows.len() == 1;
                row = rest;
            } else if let Some(rest) = r.strip_prefix("\\cline") {
                let rc: Vec<char> = rest.chars().collect();
                let s = skip_spaces(&rc, 0);
                let e = if rc.get(s) == Some(&'{') { group_end(&rc, s)? } else { 0 };
                row = &rest[rc[..e].iter().map(|c| c.len_utf8()).sum::<usize>()..];
            } else {
                row = r;
                break;
            }
        }
        if row.trim().is_empty() {
            continue;
        }
        rows.push(cells_of(row).iter().map(|c| for_telegram(c.trim())).collect::<Vec<_>>());
    }
    if rows.is_empty() {
        return None;
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    rows.iter_mut().for_each(|r| r.resize(width, String::new()));
    align.resize(width, 'c');
    Some(ArrayTable { align, header: header && rows.len() > 1, rows })
}

/// A row's cells: split at `&` outside braces and environments, not escaped.
fn cells_of(row: &str) -> Vec<String> {
    let chars: Vec<char> = row.chars().collect();
    let mut cells = Vec::new();
    let mut cur = String::new();
    let (mut braces, mut envs) = (0usize, 0usize);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let (name, after) = command(&chars, i + 1);
            match name.as_str() {
                "begin" => envs += 1,
                "end" => envs = envs.saturating_sub(1),
                _ => {}
            }
            let end = if name.is_empty() { i + 2 } else { after };
            cur.extend(&chars[i..end]);
            i = end;
            continue;
        }
        match c {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            '&' if braces == 0 && envs == 0 => {
                cells.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    cells.push(cur);
    cells
}

/// An inline formula in the LaTeX every Telegram app renders (see the module
/// docs). Several lines become one, separated by `\quad`.
pub fn for_telegram(tex: &str) -> String {
    let lines = lines_of(&rewrite_str(tex));
    lines.join(" \\quad ")
}

/// A block formula in the LaTeX every Telegram app renders: one formula per
/// line (see the module docs).
pub fn display_lines(tex: &str) -> Vec<String> {
    let lines: Vec<String> =
        lines_of(&rewrite_str(tex)).into_iter().flat_map(|l| cases_rows(&l).unwrap_or_else(|| vec![l])).collect();
    if lines.is_empty() { vec![tex.trim().to_string()] } else { lines }
}

fn rewrite_str(tex: &str) -> String {
    let chars: Vec<char> = tex.chars().collect();
    let mut out = String::with_capacity(tex.len());
    rewrite(&chars, &mut out);
    out
}

/// The end (exclusive) of the brace group starting at `i` (`chars[i] == '{'`),
/// escaped braces skipped.
fn group_end(chars: &[char], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut j = i;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 1,
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            _ => {}
        }
        j += 1;
    }
    None
}

fn skip_spaces(chars: &[char], mut i: usize) -> usize {
    while i < chars.len() && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

/// The command name at `i` (after a backslash), and where it ends.
fn command(chars: &[char], i: usize) -> (String, usize) {
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_alphabetic() {
        j += 1;
    }
    if j < chars.len() && chars[j] == '*' && j > i {
        j += 1;
    }
    (chars[i..j].iter().collect(), j)
}

/// `[optional]` at `i`, skipped.
fn skip_optional(chars: &[char], i: usize) -> usize {
    let k = skip_spaces(chars, i);
    if chars.get(k) != Some(&'[') {
        return i;
    }
    match chars[k..].iter().position(|&c| c == ']') {
        Some(p) => k + p + 1,
        None => i,
    }
}

/// The brace group at `at` (spaces skipped), rewritten, and where it ends.
fn rewritten_group(chars: &[char], at: usize) -> Option<(String, usize)> {
    let k = skip_spaces(chars, at);
    if chars.get(k) != Some(&'{') {
        return None;
    }
    let e = group_end(chars, k)?;
    let mut arg = String::new();
    rewrite(&chars[k + 1..e - 1], &mut arg);
    Some((arg, e))
}

/// The argument of `^` or `_` at `at`: a group's content, a command or one
/// character, rewritten, and where it ends.
fn script_arg(chars: &[char], at: usize) -> Option<(String, usize)> {
    let k = skip_spaces(chars, at);
    let end = match chars.get(k)? {
        '{' => return rewritten_group(chars, k),
        '\\' => match command(chars, k + 1) {
            (name, after) if !name.is_empty() => after,
            _ => (k + 2).min(chars.len()),
        },
        _ => k + 1,
    };
    let mut arg = String::new();
    rewrite(&chars[k..end], &mut arg);
    Some((arg, end))
}

fn rewrite(chars: &[char], out: &mut String) {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if !c.is_ascii() || c == '\'' {
            i = rewrite_char(chars, i, out);
            continue;
        }
        if c != '\\' || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let (name, after) = command(chars, i + 1);
        if name.is_empty() {
            // `\:` is a medium space SwiftMath does not know.
            if chars[i + 1] == ':' {
                out.push_str("\\;");
            } else {
                out.push(c);
                out.push(chars[i + 1]);
            }
            i += 2;
            continue;
        }
        if name == "begin"
            && let Some((env, body_start, body_end, end)) = environment(chars, after)
        {
            // An alignment environment: its lines (`\\`), no `&`.
            if ALIGN_ENVS.contains(&env.as_str()) {
                let mut body = String::new();
                rewrite(&chars[body_start..body_end], &mut body);
                out.push_str(&replace_alignment(&body, ""));
                i = end;
                continue;
            }
            // `array`: rows of a system in `\left\{ … \right.`, a `matrix` otherwise.
            if env == "array" {
                let mut body = String::new();
                rewrite(&chars[body_start..body_end], &mut body);
                let before = out.trim_end();
                if before.ends_with("\\left\\{") || before.ends_with("\\left\\lbrace") || before.ends_with("\\left.") {
                    out.push_str(&replace_alignment(&body, " \\quad "));
                } else {
                    out.push_str(&format!("\\begin{{matrix}}{}\\end{{matrix}}", body.trim()));
                }
                i = end;
                continue;
            }
        }
        if let Some((_, to)) = RENAMED.iter().find(|(from, _)| *from == name.trim_end_matches('*')) {
            out.push('\\');
            out.push_str(to);
            if *to == "text" {
                i = copy_text_group(chars, after, out);
                continue;
            }
            // `\mathrm{rank} A`: an operator name keeps its space before a bare argument.
            if name.starts_with("operatorname")
                && let Some((arg, e)) = rewritten_group(chars, after)
            {
                out.push('{');
                out.push_str(&arg);
                out.push('}');
                let n = skip_spaces(chars, e);
                if n > e && chars.get(n).is_some_and(|c| c.is_alphanumeric() || *c == '\\') {
                    out.push_str("\\,");
                }
                i = n.max(e);
                continue;
            }
            i = after;
            continue;
        }
        if let Some((_, to)) = RESPELLED.iter().find(|(from, _)| *from == name) {
            out.push_str(to);
            i = after;
            continue;
        }
        if let Some((_, to)) = DROPPED.iter().find(|(from, _)| *from == name) {
            let k = skip_spaces(chars, after);
            i = match (chars.get(k), group_end(chars, k)) {
                (Some('{'), Some(e)) => {
                    out.push_str(to);
                    e
                }
                _ => after,
            };
            continue;
        }
        if name == "middle" {
            // `\middle|` → `\mid`.
            let k = skip_spaces(chars, after);
            if chars.get(k) == Some(&'|') {
                out.push_str("\\mid ");
                i = k + 1;
                continue;
            }
        }
        // The argument may be one token, as TeX allows: `\pmod p` (DeepSeek, 2026-11-14).
        if let Some(template) = match name.as_str() {
            "pmod" => Some("\\quad(\\mathrm{mod}\\,#)"),
            "pod" => Some("\\quad(#)"),
            "abs" | "abs*" => Some("\\left| # \\right|"),
            "norm" | "norm*" => Some("\\left\\| # \\right\\|"),
            _ => None,
        } && let Some((arg, e)) = script_arg(chars, after)
        {
            out.push_str(&template.replace('#', &arg));
            i = e;
            continue;
        }
        // `\substack{i<j \\ j<k}` (under a sum): rows stacked with `\atop`.
        if name == "substack"
            && let Some((arg, e)) = rewritten_group(chars, after)
        {
            let rows = lines_of(&arg);
            let stacked = rows.iter().rev().fold(String::new(), |below, row| {
                if below.is_empty() { row.clone() } else { format!("{{{row} \\atop {below}}}") }
            });
            out.push_str(&stacked);
            i = e;
            continue;
        }
        if let Some(&(_, skip)) = UNWRAPPED.iter().find(|(n, _)| *n == name) {
            // `\color{red}{x}` → `x`; `\color{red} x` (a switch) → ` x`; `\boxed{x}` → `x`.
            let mut k = skip_optional(chars, after);
            let mut ok = true;
            for _ in 0..skip {
                let s = skip_spaces(chars, k);
                match (chars.get(s), group_end(chars, s)) {
                    (Some('{'), Some(e)) => k = e,
                    _ => ok = false,
                }
            }
            if ok {
                let s = skip_spaces(chars, k);
                match (chars.get(s), group_end(chars, s)) {
                    (Some('{'), Some(e)) => {
                        rewrite(&chars[s + 1..e - 1], out);
                        i = e;
                        continue;
                    }
                    _ if skip > 0 => {
                        // The switch form: what follows keeps going.
                        i = k;
                        continue;
                    }
                    // One token: `\boxed 5`.
                    _ => {
                        if let Some((arg, e)) = script_arg(chars, k) {
                            out.push_str(&arg);
                            i = e;
                            continue;
                        }
                    }
                }
            }
        }
        out.push('\\');
        out.push_str(&name);
        i = if name == "text" { copy_text_group(chars, after, out) } else { after };
    }
}

/// The text group of a `\text` at `at`, copied as written (text is not a
/// formula). Returns where it ends (`at` when there is none).
fn copy_text_group(chars: &[char], at: usize, out: &mut String) -> usize {
    let k = skip_spaces(chars, at);
    match (chars.get(k), group_end(chars, k)) {
        (Some('{'), Some(e)) => {
            out.extend(&chars[k..e]);
            e
        }
        _ => at,
    }
}

/// A prime or a character beyond ASCII at `i` (outside `\text{}`), written so
/// every app draws it. Returns where the next one starts.
fn rewrite_char(chars: &[char], i: usize, out: &mut String) -> usize {
    let c = chars[i];
    // `f'` → `f^{\prime}`, merged with a superscript after it: `f'^2` → `f^{\prime 2}`.
    if primes(c) > 0 {
        let mut n = 0;
        let mut j = i;
        while j < chars.len() && primes(chars[j]) > 0 {
            n += primes(chars[j]);
            j += 1;
        }
        let marks = "\\prime".repeat(n);
        let k = skip_spaces(chars, j);
        if chars.get(k) == Some(&'^')
            && let Some((arg, end)) = script_arg(chars, k + 1)
        {
            out.push_str(&format!("^{{{marks} {arg}}}"));
            return end;
        }
        out.push_str(&format!("^{{{marks}}}"));
        return j;
    }
    // `x²³` → `x^{23}`, `a₁` → `a_{1}`.
    if let Some((mark, _)) = script_char(c) {
        let mut j = i;
        let mut raised = String::new();
        while let Some((m, a)) = chars.get(j).and_then(|&c| script_char(c)) {
            if m != mark {
                break;
            }
            raised.push(a);
            j += 1;
        }
        out.push_str(&format!("{mark}{{{raised}}}"));
        return j;
    }
    if let Some((_, tex)) = SYMBOLS.iter().find(|(u, _)| *u == c) {
        out.push_str(tex);
        // `\times x`, not `\timesx`.
        if tex.ends_with(|c: char| c.is_ascii_alphabetic()) && chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic())
        {
            out.push(' ');
        }
        return i + 1;
    }
    if c.is_whitespace() {
        out.push(' ');
        return i + 1;
    }
    if !wants_text(c) {
        if drawn_as_is(c) {
            out.push(c);
        }
        return i + 1;
    }
    // A run of text (spaces inside kept): `\text{…}`, braced after `^` or `_`.
    let mut j = i;
    loop {
        while j < chars.len() && wants_text(chars[j]) {
            j += 1;
        }
        let k = skip_spaces(chars, j);
        if k > j && chars.get(k).is_some_and(|&c| wants_text(c)) {
            j = k;
        } else {
            break;
        }
    }
    let run: String = chars[i..j].iter().collect();
    if out.ends_with('^') || out.ends_with('_') {
        out.push_str(&format!("{{\\text{{{run}}}}}"));
    } else {
        out.push_str(&format!("\\text{{{run}}}"));
    }
    j
}

/// `{env}…\end{env}` after a `\begin`: the environment's name, its body's
/// range (past the column count or spec of `alignat` and `array`), and where
/// `\end{env}` ends.
fn environment(chars: &[char], at: usize) -> Option<(String, usize, usize, usize)> {
    let k = skip_spaces(chars, at);
    if chars.get(k) != Some(&'{') {
        return None;
    }
    let e = group_end(chars, k)?;
    let env: String = chars[k + 1..e - 1].iter().collect();
    let mut body_start = e;
    if env.starts_with("alignat") || env == "alignedat" || env == "array" {
        let s = skip_spaces(chars, e);
        if chars.get(s) == Some(&'{') {
            body_start = group_end(chars, s)?;
        }
    }
    // The matching `\end{env}`, nested environments of the same name counted.
    let (open, close): (Vec<char>, Vec<char>) =
        (format!("\\begin{{{env}}}").chars().collect(), format!("\\end{{{env}}}").chars().collect());
    let mut depth = 1usize;
    let mut j = body_start;
    while j < chars.len() {
        if chars[j..].starts_with(&close) {
            depth -= 1;
            if depth == 0 {
                return Some((env, body_start, j, j + close.len()));
            }
            j += close.len();
        } else if chars[j..].starts_with(&open) {
            depth += 1;
            j += open.len();
        } else {
            j += 1;
        }
    }
    None
}

/// A body's alignment points or columns (`&` outside nested environments, not
/// escaped) replaced with `with`.
fn replace_alignment(body: &str, with: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut depth = 0usize;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let (name, after) = command(&chars, i + 1);
            match name.as_str() {
                "begin" => depth += 1,
                "end" => depth = depth.saturating_sub(1),
                _ => {}
            }
            let end = if name.is_empty() { i + 2 } else { after };
            out.extend(&chars[i..end]);
            i = end;
            continue;
        }
        if chars[i] == '&' && depth == 0 {
            // Columns: one `\quad` between, whatever the spacing around `&`.
            if !with.is_empty() {
                out.truncate(out.trim_end().len());
                out.push_str(with);
                i = skip_spaces(&chars, i + 1);
                continue;
            }
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.trim().to_string()
}

/// Where the delimiter after `\left` or `\right` (ending at `at`) ends.
fn delimiter_end(chars: &[char], at: usize) -> usize {
    let k = skip_spaces(chars, at);
    match chars.get(k) {
        Some('\\') => match command(chars, k + 1) {
            (name, after) if !name.is_empty() => after,
            _ => (k + 2).min(chars.len()),
        },
        Some(_) => k + 1,
        None => k,
    }
}

/// The lines of a formula: split at `\\` outside braces and environments
/// (`\\[2pt]` spacing dropped), each trimmed, empty ones left out. A
/// `\left … \right` pair around a line break goes (a system's brace).
fn lines_of(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut dropped = vec![false; chars.len()];
    // The `\left` delimiters open (their range, whether a line break is inside).
    let mut open: Vec<(usize, usize, bool)> = Vec::new();
    let (mut braces, mut envs) = (0usize, 0usize);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            if chars[i + 1] == '\\' {
                if braces == 0 && envs == 0 {
                    open.iter_mut().for_each(|o| o.2 = true);
                }
                i += 2;
                continue;
            }
            let (name, after) = command(&chars, i + 1);
            let top = braces == 0 && envs == 0;
            match name.as_str() {
                "begin" => envs += 1,
                "end" => envs = envs.saturating_sub(1),
                "left" if top => {
                    let e = delimiter_end(&chars, after);
                    open.push((i, e, false));
                    i = e;
                    continue;
                }
                "right" if top => {
                    let e = delimiter_end(&chars, after);
                    if let Some((s, se, true)) = open.pop() {
                        dropped[s..se].fill(true);
                        dropped[i..e].fill(true);
                    }
                    i = e;
                    continue;
                }
                _ => {}
            }
            i = if name.is_empty() { i + 2 } else { after };
            continue;
        }
        match c {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }

    let mut lines = Vec::new();
    let mut cur = String::new();
    let (mut braces, mut envs) = (0usize, 0usize);
    let mut i = 0;
    while i < chars.len() {
        if dropped[i] {
            i += 1;
            continue;
        }
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            if chars[i + 1] == '\\' && braces == 0 && envs == 0 {
                lines.push(std::mem::take(&mut cur));
                i = skip_optional(&chars, i + 2);
                continue;
            }
            let (name, after) = command(&chars, i + 1);
            match name.as_str() {
                "begin" => envs += 1,
                "end" => envs = envs.saturating_sub(1),
                _ => {}
            }
            let end = if name.is_empty() { i + 2 } else { after };
            cur.extend(&chars[i..end]);
            i = end;
            continue;
        }
        match c {
            '{' => braces += 1,
            '}' => braces = braces.saturating_sub(1),
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    lines.push(cur);
    lines.into_iter().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect()
}

/// A line with one `cases` environment at its top level: one line per case,
/// `prefix case,\quad condition suffix` (the suffix on the last line only).
fn cases_rows(s: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = s.chars().collect();
    let begins: Vec<(usize, &str)> = (0..chars.len())
        .filter_map(|i| {
            CASES_ENVS
                .iter()
                .find(|env| chars[i..].starts_with(&format!("\\begin{{{env}}}").chars().collect::<Vec<_>>()))
                .map(|env| (i, *env))
        })
        .collect();
    let &[(start, _)] = begins.as_slice() else { return None };
    // Only at the top level: not inside braces.
    let mut depth = 0usize;
    for &c in &chars[..start] {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    if depth != 0 {
        return None;
    }
    let (_, body_start, body_end, end) = environment(&chars, start + "\\begin".len())?;
    let prefix: String = chars[..start].iter().collect();
    let suffix: String = chars[end..].iter().collect();
    let body: String = chars[body_start..body_end].iter().collect();
    let rows = lines_of(&body);
    let n = rows.len();
    let out: Vec<String> = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            // `value & condition` → `value,\quad condition` (one comma).
            let row = match row.split_once('&') {
                Some((v, c)) if !v.ends_with('\\') => {
                    let v = v.trim();
                    let comma = if v.ends_with(',') || v.ends_with(';') { "" } else { "," };
                    format!("{v}{comma}\\quad {}", c.trim())
                }
                _ => row,
            };
            let tail = if i + 1 == n { suffix.trim() } else { "" };
            format!("{} {row} {tail}", prefix.trim()).trim().to_string()
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::{display_lines as d, for_telegram as t};

    #[test]
    fn what_every_app_renders() {
        // As written.
        for s in ["x^2 + 1", r"\frac{a}{b}", r"\{1, 2\}", r"\text{or}", r"\mathbb{R}", r"\,\;\!", r"x \& y"] {
            assert_eq!(t(s), s);
        }
        assert_eq!(t(r"\dfrac{5 \pm 1}{2} \implies x"), r"\frac{5 \pm 1}{2} \implies x");
        assert_eq!(t(r"\tfrac12 + \dbinom{n}{k}"), r"\frac12 + \binom{n}{k}");
        assert_eq!(
            t(r"\operatorname{lcm}(a,b) + \operatorname*{arg\,max}_x"),
            r"\mathrm{lcm}(a,b) + \mathrm{arg\,max}_x"
        );
        assert_eq!(t(r"\operatorname{rank} A"), r"\mathrm{rank}\,A");
        assert_eq!(t(r"a \equiv b \pmod{p}"), r"a \equiv b \quad(\mathrm{mod}\,p)");
        // One token as the argument (DeepSeek's `2^n\equiv -1\pmod p.`).
        assert_eq!(t(r"2^n\equiv -1\pmod p."), r"2^n\equiv -1\quad(\mathrm{mod}\,p).");
        assert_eq!(t(r"x \pmod{2^k} \pod\ell"), r"x \quad(\mathrm{mod}\,2^k) \quad(\ell)");
        assert_eq!(t(r"a \bmod b = a \mod b"), r"a \;\mathrm{mod}\; b = a \quad\mathrm{mod}\; b");
        assert_eq!(t(r"\lcm(a,b) \coloneqq c"), r"\mathrm{lcm}(a,b) := c");
        assert_eq!(t(r"\abs{x} + \norm{v} + \abs*{y}"), r"\left| x \right| + \left\| v \right\| + \left| y \right|");
        assert_eq!(t(r"\sum_{\substack{i<j \\ i,j \in S}} a_i"), r"\sum_{{i<j \atop i,j \in S}} a_i");
        assert_eq!(t(r"\therefore x \rightleftharpoons y \S 2"), r"\text{∴} x \text{⇌} y \text{§} 2");
        assert_eq!(t(r"\phantom{0}1 \boxed 5"), "1 5");
        assert_eq!(t(r"\textbf{bold} \textit{it} \mbox{box}"), r"\text{bold} \text{it} \text{box}");
        assert_eq!(t(r"\lvert x \rvert + \lVert v \rVert"), r"| x | + \| v \|");
        assert_eq!(t(r"1, 2, \dots, n"), r"1, 2, \ldots, n");
        assert_eq!(t(r"a \: b \hspace{1em} c"), r"a \; b \quad  c");
        assert_eq!(t(r"x = 1 \tag{3}"), "x = 1");
        assert_eq!(t(r"\mathscr{L} + \bm{v}"), r"\mathcal{L} + \boldsymbol{v}");
        assert_eq!(t(r"\left( a \middle| b \right)"), r"\left( a \mid  b \right)");
        // Boxes and colours: their content.
        assert_eq!(t(r"\boxed{44}."), "44.");
        assert_eq!(t(r"\boxed{x = \dfrac{1}{2} \text{ or } \{y\}}"), r"x = \frac{1}{2} \text{ or } \{y\}");
        assert_eq!(t(r"\fbox{a} \framebox[2cm]{b}"), "a b");
        assert_eq!(t(r"\color{red}{x} + \textcolor{blue}{y}"), "x + y");
        assert_eq!(t(r"{\color{red} x} + 1"), "{ x} + 1");
        // Inline lines: one line, `\quad` between.
        assert_eq!(t(r"a = b \\ c = d"), r"a = b \quad c = d");
        // Unclosed or unknown: as written.
        assert_eq!(t(r"\boxed{x"), r"\boxed{x");
        assert_eq!(t(r"\boxedx"), r"\boxedx");
    }

    #[test]
    fn primes_and_text_beyond_ascii() {
        // Primes: superscript primes, merged with a superscript after them.
        assert_eq!(t("f'(x) + f''(x)"), r"f^{\prime}(x) + f^{\prime\prime}(x)");
        assert_eq!(t("y'^2 + y′"), r"y^{\prime 2} + y^{\prime}");
        assert_eq!(t(r"g'^{\alpha}"), r"g^{\prime \alpha}");
        // Symbols: their commands, spaced before a letter.
        assert_eq!(t("3 × 4 ≤ 5·x ≠ π"), r"3 \times 4 \le 5\cdot x \ne \pi");
        assert_eq!(t("a×b → ∞, 90°, x ∈ ℝ"), r"a\times b \to \infty, 90^{\circ}, x \in \mathbb{R}");
        assert_eq!(t("x² + y₁₂ − 1"), "x^{2} + y_{12} - 1");
        // Other text: `\text{}`, spaces inside kept; `\text{}` itself as written.
        assert_eq!(t("v = 100米 / 10 秒"), r"v = 100\text{米} / 10 \text{秒}");
        assert_eq!(t("速度 = 距离 / 时间"), r"\text{速度} = \text{距离} / \text{时间}");
        assert_eq!(t("最 大 值"), r"\text{最 大 值}");
        assert_eq!(t("x_最"), r"x_{\text{最}}");
        assert_eq!(t(r"\text{当 x≥0 时, f'(x)}"), r"\text{当 x≥0 时, f'(x)}");
        assert_eq!(t(r"\textbf{注意：} x"), r"\text{注意：} x");
        assert_eq!(t(r"\mathrm{最大}"), r"\mathrm{\text{最大}}");
        // Drawn as they are: accented Latin and Cyrillic.
        assert_eq!(t("é + д"), "é + д");
    }

    #[test]
    fn a_formula_that_is_an_array_is_a_table() {
        use super::{ArrayTable, array_table};
        // DeepSeek's table of primes (2026-11-14), shortened.
        let tex = "\n\\begin{array}{c|c}\np & n\\equiv\\pmod p\\\\ \\hline\n41 & 0,40\\\\\n43 & 1,41\n\\end{array}\n";
        assert_eq!(
            array_table(tex),
            Some(ArrayTable {
                align: vec!['c', 'c'],
                header: true,
                rows: vec![
                    vec!["p".into(), r"n\equiv\quad(\mathrm{mod}\,p)".into()],
                    vec!["41".into(), "0,40".into()],
                    vec!["43".into(), "1,41".into()],
                ],
            })
        );
        // Column arguments skipped, short rows padded, no rule: no header.
        assert_eq!(
            array_table(r"\begin{array}{@{}r p{2cm}l@{}} 1 & \frac{a}{b} & x \\ 2 \end{array}"),
            Some(ArrayTable {
                align: vec!['r', 'l', 'l'],
                header: false,
                rows: vec![vec!["1".into(), r"\frac{a}{b}".into(), "x".into()], vec!["2".into(), "".into(), "".into()]],
            })
        );
        // Not only an array: a formula.
        assert_eq!(array_table(r"A = \begin{array}{cc} 1 & 2 \end{array}"), None);
        assert_eq!(array_table(r"\begin{array}{cc} 1 & 2 \end{array}."), None);
        assert_eq!(array_table(r"\begin{pmatrix} 1 & 2 \end{pmatrix}"), None);
    }

    #[test]
    fn block_formulas_become_one_per_line() {
        assert_eq!(d(r"x^2"), ["x^2"]);
        // Alignment environments: their lines, without `&`.
        assert_eq!(d("\\begin{aligned}\na &= b + c \\\\\nd &= e\n\\end{aligned}"), ["a = b + c", "d = e"]);
        assert_eq!(d(r"\begin{align*} x &= 1 \\[2pt] y &= 2 \nonumber \end{align*}"), ["x = 1", "y = 2"]);
        assert_eq!(d(r"\begin{alignat}{2} x &= 1 & y &= 2 \end{alignat}"), ["x = 1  y = 2"]);
        assert_eq!(d(r"a = b \\ c = d"), ["a = b", "c = d"]);
        // A box around lines (DeepSeek's answer, 2026-11-14).
        assert_eq!(
            d("\\boxed{\n\\begin{aligned}\n&40,41,44,\\\\\n&121,122\n\\end{aligned}}"),
            ["40,41,44,", "121,122"]
        );
        // Nested matrices keep their rows and `&`.
        assert_eq!(
            d(r"\begin{align*} A &= \begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix} \end{align*}"),
            [r"A = \begin{pmatrix} 1 & 2 \\ 3 & 4 \end{pmatrix}"]
        );
        // `cases`: one line per case, the condition after `,\quad`.
        assert_eq!(
            d(r"f(x) = \begin{cases} 1 & x > 0 \\ 0 & x \le 0 \end{cases}."),
            [r"f(x) = 1,\quad x > 0", r"f(x) = 0,\quad x \le 0 ."]
        );
        assert_eq!(d(r"\begin{cases} x + y = 1 \\ x - y = 0 \end{cases}"), ["x + y = 1", "x - y = 0"]);
        assert_eq!(
            d(r"f(x) = \begin{cases} x^2, & x \ge 0 \\ -x; & x < 0 \end{cases}"),
            [r"f(x) = x^2,\quad x \ge 0", r"f(x) = -x;\quad x < 0"]
        );
        assert_eq!(
            d(
                r"\begin{aligned} f(x) &= \begin{cases} 1 & x > 0 \\ 0 & \text{else} \end{cases} \\ g &= 2 \end{aligned}"
            ),
            [r"f(x) = 1,\quad x > 0", r"f(x) = 0,\quad \text{else}", "g = 2"]
        );
        // Inside braces or twice: as written.
        assert_eq!(d(r"\sqrt{\begin{cases} a \end{cases}}"), [r"\sqrt{\begin{cases} a \end{cases}}"]);
        // Systems in `\left\{ … \right.`: their lines, without the brace.
        assert_eq!(
            d(r"\left\{ \begin{aligned} x + y &= 1 \\ x - y &= 0 \end{aligned} \right."),
            ["x + y = 1", "x - y = 0"]
        );
        assert_eq!(
            d(r"\left\{\begin{array}{ll} x + y = 1 & (1) \\ x - y = 0 & (2) \end{array}\right."),
            [r"x + y = 1 \quad (1)", r"x - y = 0 \quad (2)"]
        );
        // Other delimiters stay when no line break is inside them.
        assert_eq!(d(r"\left( \frac{a}{b} \right)^2"), [r"\left( \frac{a}{b} \right)^2"]);
        // `\newline` is a line break; table rules go.
        assert_eq!(d(r"a \newline b"), ["a", "b"]);
        assert_eq!(
            d(r"\begin{array}{c|c} \hline 1 & 2 \\ \hline 3 & 4 \end{array}"),
            [r"\begin{matrix}1 & 2 \\  3 & 4\end{matrix}"]
        );
        // `array` otherwise: a matrix.
        assert_eq!(
            d(r"\left( \begin{array}{cc} 1 & 2 \\ 3 & 4 \end{array} \right)"),
            [r"\left( \begin{matrix}1 & 2 \\ 3 & 4\end{matrix} \right)"]
        );
    }
}
