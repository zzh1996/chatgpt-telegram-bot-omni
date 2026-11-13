//! Tool calls shown readably instead of as bare JSON (the owner's request).
//!
//! Providers report their tool calls verbatim ([`ToolCall`] input and output
//! are the API's own objects). The shapes the official APIs share become
//! text, links and code blocks: search queries and the sources found, pages
//! opened or searched, fetched URLs and their status, code and what it
//! printed (stdout, stderr, exit code), files, errors, image-generation
//! settings. A value no shape matches is shown as its JSON, and so is every
//! field a matched shape does not show, so nothing is lost. Only opaque
//! blobs (encrypted content, favicons) are left out; `/dump` has everything.

use omni_core::content::ToolCall;
use omni_markdown::render::is_safe_url;
use omni_markdown::rich::{RBlock, RItem, RText};
use serde_json::{Map, Value};

/// Fields with nothing a reader can use (opaque or decorative).
const OPAQUE: [&str; 5] = ["encrypted_content", "encrypted_index", "icon", "signature", "thoughtSignature"];

/// The blocks of one tool call in its fold: what it was given, then what it
/// returned (each readable when its shape is known, else its JSON).
pub fn blocks(t: &ToolCall) -> Vec<RBlock> {
    let mut out = Vec::new();
    if let Some(i) = t.input.as_ref().filter(|v| !v.is_null()) {
        out.extend(input(t, i));
    }
    if let Some(o) = t.output.as_ref().filter(|v| !v.is_null()) {
        out.extend(output(t, o));
    }
    out
}

/// How a call ended, when that belongs in the fold's title: `exit 1`,
/// `error: max_uses_exceeded`, `timed out`, `aborted: …`, a URL status
/// (`paywall`), a failed outcome. `None` for a normal end.
pub fn outcome(t: &ToolCall) -> Option<String> {
    let o = t.output.as_ref()?;
    let mut found = Vec::new();
    outcomes(o, &mut found, 0);
    found.dedup();
    (!found.is_empty()).then(|| found.join(", "))
}

fn outcomes(v: &Value, found: &mut Vec<String>, depth: usize) {
    if depth > 3 {
        return;
    }
    match v {
        Value::Object(o) => {
            if let Some(c) = o.get("error_code").and_then(Value::as_str) {
                found.push(format!("error: {c}"));
            }
            if let Some(e) = o.get("error").filter(|e| !e.is_null()) {
                let code = e.get("code").and_then(Value::as_str).or_else(|| e.as_str()).unwrap_or("error");
                found.push(format!("error: {code}"));
            }
            for k in ["return_code", "exit_code"] {
                if let Some(c) = o.get(k).and_then(Value::as_i64).filter(|c| *c != 0) {
                    found.push(format!("exit {c}"));
                }
            }
            if o.get("command_timed_out").and_then(Value::as_bool) == Some(true) {
                found.push("timed out".into());
            }
            if let Some(a) = o.get("abort_reason").filter(|a| !a.is_null()) {
                found.push(format!("aborted: {}", a.as_str().map(str::to_string).unwrap_or_else(|| a.to_string())));
            }
            if let Some(oc) = o.get("outcome").and_then(Value::as_str).filter(|x| *x != "OUTCOME_OK") {
                found.push(oc.trim_start_matches("OUTCOME_").to_ascii_lowercase().replace('_', " "));
            }
            if let Some(st) = o.get("urlRetrievalStatus").and_then(Value::as_str) {
                let s = url_status(st);
                if s != "success" {
                    found.push(s);
                }
            }
            for k in ["url_metadata", "urlMetadata", "content", "results"] {
                if let Some(x) = o.get(k) {
                    outcomes(x, found, depth + 1);
                }
            }
        }
        Value::Array(a) => {
            for x in a {
                outcomes(x, found, depth + 1);
            }
        }
        // xAI code logs: a JSON string.
        Value::String(s) if s.trim_start().starts_with('{') => {
            if let Ok(x) = serde_json::from_str::<Value>(s) {
                outcomes(&x, found, depth + 1);
            }
        }
        _ => {}
    }
    if let Some(logs) = v.get("logs").and_then(Value::as_str)
        && let Ok(x) = serde_json::from_str::<Value>(logs.trim())
        && x.is_object()
    {
        outcomes(&x, found, depth + 1);
    }
}

fn url_status(s: &str) -> String {
    s.trim_start_matches("URL_RETRIEVAL_STATUS_").to_ascii_lowercase().replace('_', " ")
}

fn is_code_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("code") || n.contains("interpreter") || n.contains("python")
}

fn text(s: &str) -> RBlock {
    RBlock::Paragraph(RText::plain(s))
}

fn json_block(v: &Value) -> RBlock {
    RBlock::Pre { text: serde_json::to_string_pretty(v).unwrap_or_default(), language: "json".into() }
}

/// Fields no shape took: a few flat values as one line (`duration: 1 ·
/// resolution: 720p`, nested ones dotted: `citations.enabled: true`), anything
/// deeper, longer or more numerous as its JSON.
fn rest_block(v: &Value) -> RBlock {
    fn leaves(prefix: &str, v: &Value, depth: usize, out: &mut Vec<String>) -> bool {
        match v {
            Value::String(s) if s.chars().count() <= 120 && !s.contains('\n') => {
                out.push(format!("{prefix}: {s}"));
                true
            }
            Value::Number(_) | Value::Bool(_) => {
                out.push(format!("{prefix}: {v}"));
                true
            }
            Value::Object(o) if depth < 2 && !o.is_empty() => {
                o.iter().all(|(k, x)| leaves(&format!("{prefix}.{k}"), x, depth + 1, out))
            }
            _ => false,
        }
    }
    if let Some(o) = v.as_object() {
        let mut line = Vec::new();
        if o.iter().all(|(k, x)| leaves(k, x, 0, &mut line)) && line.len() <= 8 {
            return text(&line.join(" · "));
        }
    }
    json_block(v)
}

fn code(s: &str, language: &str) -> RBlock {
    RBlock::Pre { text: s.trim_end_matches('\n').to_string(), language: language.into() }
}

fn link(label: &str, url: &str) -> RText {
    let label = if label.trim().is_empty() { url } else { label };
    if is_safe_url(url) {
        RText::Url { text: Box::new(RText::plain(label)), url: url.to_string() }
    } else {
        RText::plain(format!("{label} ({url})"))
    }
}

fn short(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max { format!("{}…", s.chars().take(max - 1).collect::<String>()) } else { s }
}

/// An object whose fields are taken one by one; the rest is shown as JSON.
struct Fields<'a> {
    map: &'a Map<String, Value>,
    used: Vec<&'a str>,
}

impl<'a> Fields<'a> {
    fn new(map: &'a Map<String, Value>) -> Self {
        Self { map, used: Vec::new() }
    }
    fn get(&mut self, k: &'a str) -> Option<&'a Value> {
        let v = self.map.get(k).filter(|v| !v.is_null())?;
        self.used.push(k);
        Some(v)
    }
    fn str(&mut self, k: &'a str) -> Option<&'a str> {
        let v = self.map.get(k)?.as_str()?;
        self.used.push(k);
        Some(v).filter(|s| !s.trim().is_empty())
    }
    fn skip(&mut self, k: &'a str) {
        self.used.push(k);
    }
    /// The fields nobody took (nulls and opaque blobs aside).
    fn rest(&self) -> Option<Value> {
        let m: Map<String, Value> = self
            .map
            .iter()
            .filter(|(k, v)| !v.is_null() && !self.used.contains(&k.as_str()) && !OPAQUE.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        (!m.is_empty()).then_some(Value::Object(m))
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => vec![s.clone()],
        Value::Array(a) => {
            a.iter().filter_map(|x| x.as_str()).filter(|s| !s.trim().is_empty()).map(String::from).collect()
        }
        _ => vec![],
    }
}

// ---- inputs ----

fn input(t: &ToolCall, v: &Value) -> Vec<RBlock> {
    match v {
        Value::String(s) if is_code_tool(&t.name) => vec![code(s, "python")],
        Value::String(s) if t.name.ends_with("_generation") => {
            vec![RBlock::Quote { blocks: vec![text(s)] }]
        }
        Value::String(s) => vec![text(s)],
        Value::Object(o) => input_object(t, o),
        other => vec![json_block(other)],
    }
}

fn input_object(t: &ToolCall, o: &Map<String, Value>) -> Vec<RBlock> {
    let mut f = Fields::new(o);
    let mut out = Vec::new();
    let kind = f.str("type").map(str::to_string);
    if o.get("id").and_then(Value::as_str) == t.id.as_deref() {
        f.skip("id");
    }
    // Searches: the query or queries (OpenAI/xAI send both, the first query among them).
    let mut queries = f.get("queries").map(strings).unwrap_or_default();
    if let Some(q) = f.str("query")
        && !queries.iter().any(|x| x == q)
    {
        queries.insert(0, q.to_string());
    }
    for q in &queries {
        out.push(text(&format!("🔎 {q}")));
    }
    for q in f.get("image_queries").map(strings).unwrap_or_default() {
        out.push(text(&format!("🖼 {q}")));
    }
    // Pages opened or searched in (OpenAI/xAI web search actions).
    let url = f.str("url").map(str::to_string);
    match (kind.as_deref(), f.str("pattern")) {
        (_, Some(p)) => {
            let mut parts = vec![RText::plain(format!("find “{p}”"))];
            if let Some(u) = &url {
                parts.push(RText::plain(" in "));
                parts.push(link(u, u));
            }
            out.push(RBlock::Paragraph(RText::Concat(parts)));
        }
        (Some("open_page" | "open"), None) if url.is_some() => {
            let u = url.as_deref().unwrap_or_default();
            out.push(RBlock::Paragraph(RText::Concat(vec![RText::plain("open "), link(u, u)])));
        }
        (_, None) if let Some(u) = &url => out.push(RBlock::Paragraph(link(u, u))),
        _ => {}
    }
    if let Some(urls) = f.get("urls").map(strings).filter(|u| !u.is_empty()) {
        out.push(RBlock::List {
            items: urls.iter().map(|u| RItem { checkbox: None, blocks: vec![RBlock::Paragraph(link(u, u))] }).collect(),
        });
    }
    // Code, shell commands, file edits.
    let language = f.str("language").map(|l| l.to_ascii_lowercase());
    if let Some(c) = f.str("code") {
        out.push(code(c, language.as_deref().unwrap_or("python")));
    }
    let path = f.str("path").map(str::to_string);
    if let Some(cmd) = f.get("command") {
        match (cmd.as_str(), &path) {
            // Text editor: `create /tmp/plot.py` and the content.
            (Some(c), Some(p)) => out.push(text(&format!("{c} {p}"))),
            (Some(c), None) => out.push(code(c, "bash")),
            _ => out.push(json_block(cmd)),
        }
    } else if let Some(p) = &path {
        out.push(text(p));
    }
    if let Some(cmds) = f.get("commands") {
        for c in strings(cmds) {
            out.push(code(&c, "bash"));
        }
    }
    let ext = path.as_deref().and_then(|p| p.rsplit_once('.')).map(|(_, e)| e.to_string()).unwrap_or_default();
    for k in ["file_text", "new_str", "insert_text"] {
        if let Some(s) = f.str(k) {
            out.push(code(s, &ext));
        }
    }
    if let Some(s) = f.str("old_str") {
        out.push(text("replacing:"));
        out.push(code(s, &ext));
    }
    // A call made by code (Claude's programmatic tool calling): who called it.
    match f.get("caller") {
        Some(Value::String(c)) => out.push(text(&format!("↳ called from {c}"))),
        Some(Value::Object(c)) if c.get("type").and_then(Value::as_str).is_some_and(|t| t != "direct") => {
            let ty = c.get("type").and_then(Value::as_str).unwrap_or_default();
            // `code_execution_20260120` → `code_execution`
            let name =
                ty.rsplit_once('_').filter(|(_, d)| d.chars().all(|x| x.is_ascii_digit())).map_or(ty, |(n, _)| n);
            out.push(text(&format!("↳ called from {name}")));
        }
        Some(Value::Object(_)) | None => {}
        Some(other) => out.push(json_block(other)),
    }
    if let Some(p) = f.str("prompt") {
        out.push(RBlock::Quote { blocks: vec![text(p)] });
    }
    // A kind other than the plain search we described.
    if let Some(k) =
        kind.as_deref().filter(|k| !matches!(*k, "search" | "open_page" | "open" | "find_in_page" | "find"))
    {
        out.insert(0, text(k));
    }
    if let Some(rest) = f.rest() {
        out.push(rest_block(&rest));
    }
    out
}

// ---- outputs ----

fn output(t: &ToolCall, v: &Value) -> Vec<RBlock> {
    match v {
        Value::Array(a) if !a.is_empty() && a.iter().all(is_source) => vec![sources(a)],
        Value::Array(a) if !a.is_empty() && a.iter().all(|x| x.get("type").is_some()) => {
            a.iter().flat_map(|x| output_item(t, x)).collect()
        }
        Value::Object(o) => output_object(t, o),
        Value::String(s) if is_code_tool(&t.name) => vec![code(s, "")],
        Value::String(s) => vec![text(s)],
        other => vec![json_block(other)],
    }
}

/// A search result or source: something with a link.
fn is_source(v: &Value) -> bool {
    v.get("url").or_else(|| v.get("link")).and_then(Value::as_str).is_some()
}

/// Sources as a numbered list: the title (linked), where and when, a short
/// excerpt when the result carries text, any other field as JSON.
fn sources(list: &[Value]) -> RBlock {
    let items = list
        .iter()
        .map(|s| {
            let Some(o) = s.as_object() else { return RItem { checkbox: None, blocks: vec![json_block(s)] } };
            let mut f = Fields::new(o);
            f.skip("type");
            let url = f.str("url").or_else(|| f.str("link")).unwrap_or_default().to_string();
            let title = f.str("title").or_else(|| f.str("name")).unwrap_or_default().to_string();
            let mut line = Vec::new();
            if let Some(r) = f.str("refer") {
                line.push(RText::plain(format!("[{r}] ")));
            }
            line.push(link(&title, &url));
            let mut about = Vec::new();
            for k in ["media", "site_name", "source"] {
                if let Some(m) = f.str(k) {
                    about.push(m.to_string());
                }
            }
            for k in ["publish_date", "publish_time", "page_age", "date", "published_date"] {
                if let Some(d) = f.str(k) {
                    about.push(d.to_string());
                }
            }
            if !about.is_empty() {
                line.push(RText::plain(format!(" · {}", about.join(" · "))));
            }
            let mut blocks = vec![RBlock::Paragraph(RText::Concat(line))];
            for k in ["content", "snippet", "text", "summary", "cited_text"] {
                if let Some(c) = f.str(k) {
                    blocks.push(RBlock::Footer(RText::plain(short(c, 200))));
                }
            }
            if let Some(rest) = f.rest() {
                blocks.push(rest_block(&rest));
            }
            RItem { checkbox: None, blocks }
        })
        .collect();
    RBlock::OrderedList { start: 1, items }
}

/// One element of a list of typed outputs (code-interpreter `logs`/`image`, result blocks).
fn output_item(t: &ToolCall, v: &Value) -> Vec<RBlock> {
    let Some(o) = v.as_object() else { return vec![json_block(v)] };
    match o.get("type").and_then(Value::as_str) {
        Some("logs") if o.len() == 2 && o.get("logs").is_some_and(Value::is_string) => {
            logs(o.get("logs").and_then(Value::as_str).unwrap_or_default())
        }
        Some("image") => {
            let mut f = Fields::new(o);
            f.skip("type");
            f.skip("url");
            let mut out = vec![text("image (shown in the reply)")];
            if let Some(rest) = f.rest() {
                out.push(rest_block(&rest));
            }
            out
        }
        _ => output_object(t, o),
    }
}

/// Code-interpreter logs: plain text (a fenced block unwrapped), or xAI's JSON
/// `{stdout, stderr, exit_code, command_timed_out}`.
fn logs(s: &str) -> Vec<RBlock> {
    let trimmed = s.trim();
    if trimmed.starts_with('{')
        && let Ok(Value::Object(o)) = serde_json::from_str::<Value>(trimmed)
        && (o.contains_key("stdout") || o.contains_key("stderr"))
    {
        return code_result(&o);
    }
    let unfenced = trimmed
        .strip_prefix("```")
        .and_then(|x| x.split_once('\n').map(|(_, body)| body))
        .and_then(|x| x.trim_end().strip_suffix("```"))
        .unwrap_or(s);
    if unfenced.trim().is_empty() { vec![] } else { vec![code(unfenced, "")] }
}

/// stdout, stderr and how the run ended (Anthropic's code/bash results, xAI logs).
fn code_result(o: &Map<String, Value>) -> Vec<RBlock> {
    let mut f = Fields::new(o);
    f.skip("type");
    let mut out = Vec::new();
    if let Some(s) = f.str("stdout") {
        out.push(code(s, ""));
    } else {
        f.skip("stdout");
    }
    if let Some(s) = f.str("stderr") {
        out.push(text("stderr:"));
        out.push(code(s, ""));
    } else {
        f.skip("stderr");
    }
    for k in ["return_code", "exit_code"] {
        if let Some(c) = f.get(k).and_then(Value::as_i64)
            && c != 0
        {
            out.push(text(&format!("exit {c}")));
        }
    }
    if let Some(b) = f.get("command_timed_out").and_then(Value::as_bool)
        && b
    {
        out.push(text("timed out"));
    }
    if let Some(a) = f.get("abort_reason") {
        out.push(text(&format!("aborted: {}", a.as_str().map(str::to_string).unwrap_or_else(|| a.to_string()))));
    }
    // Files the run created (Anthropic: `{type: …_output, file_id}`).
    if let Some(files) = f.get("content").and_then(Value::as_array) {
        for x in files {
            let meta = x.get("file");
            let name = meta.and_then(|m| m.get("filename")).and_then(Value::as_str);
            let size = meta.and_then(|m| m.get("size_bytes").or_else(|| m.get("size"))).and_then(Value::as_u64);
            match (name, x.get("file_id").and_then(Value::as_str)) {
                (Some(n), _) => out.push(text(&format!(
                    "📎 {n}{}",
                    size.map(|b| format!(" ({})", crate::compose::fmt_bytes(b))).unwrap_or_default()
                ))),
                (None, Some(id)) => out.push(text(&format!("📎 file {id}"))),
                _ => out.push(json_block(x)),
            }
        }
    }
    if let Some(rest) = f.rest() {
        out.push(rest_block(&rest));
    }
    out
}

fn output_object(t: &ToolCall, o: &Map<String, Value>) -> Vec<RBlock> {
    // Errors (Anthropic `*_tool_result_error`, OpenAI `error`).
    if o.contains_key("error_code") || o.get("error").is_some_and(|e| !e.is_null()) {
        let mut f = Fields::new(o);
        f.skip("type");
        let mut line = String::from("⚠️ ");
        if let Some(c) = f.str("error_code") {
            line.push_str(c);
        }
        if let Some(e) = f.get("error") {
            let code = e.get("code").and_then(Value::as_str).or_else(|| e.as_str());
            let msg = e.get("message").and_then(Value::as_str);
            line.push_str(&[code, msg].into_iter().flatten().collect::<Vec<_>>().join(": "));
            if code.is_none() && msg.is_none() {
                line.push_str(&e.to_string());
            }
        }
        if let Some(m) = f.str("error_message").or_else(|| f.str("message")) {
            line.push_str(&format!(": {m}"));
        }
        let mut out = vec![text(&line)];
        if let Some(rest) = f.rest() {
            out.push(rest_block(&rest));
        }
        return out;
    }
    if o.contains_key("stdout") || o.contains_key("stderr") || o.contains_key("return_code") {
        return code_result(o);
    }
    let mut f = Fields::new(o);
    let mut out = Vec::new();
    if o.get("id").and_then(Value::as_str) == t.id.as_deref() {
        f.skip("id");
    }
    // Gemini code execution: `{outcome, output}`.
    if let Some(oc) = f.str("outcome") {
        if let Some(s) = f.str("output") {
            out.push(code(s, ""));
        } else {
            f.skip("output");
        }
        if oc != "OUTCOME_OK" {
            out.push(text(&format!("outcome: {}", oc.trim_start_matches("OUTCOME_").to_ascii_lowercase())));
        }
    }
    // Sources found (OpenAI/xAI/Kimi `{sources: [...]}`), results.
    for k in ["sources", "results"] {
        if let Some(Value::Array(a)) = f.get(k) {
            if !a.is_empty() && a.iter().all(is_source) {
                out.push(sources(a));
            } else if !a.is_empty() {
                out.push(json_block(&Value::Array(a.clone())));
            }
        }
    }
    // Google Search Suggestions: the chips, linked exactly as given — a list of
    // `{text, url}`, or the widget's HTML (Gemini's tool responses), whose
    // styling is not information.
    if let Some(sg) = f.get("search_suggestions") {
        let chips: Vec<(String, String)> = match sg {
            Value::Array(a) => a
                .iter()
                .filter_map(|c| Some((c.get("text")?.as_str()?.to_string(), c.get("url")?.as_str()?.to_string())))
                .collect(),
            Value::String(html) => omni_llm::google::stream::search_chips(html),
            _ => vec![],
        };
        let complete = match sg {
            Value::Array(a) => chips.len() == a.len(),
            _ => true,
        };
        if complete && !chips.is_empty() {
            let mut line = vec![RText::plain("Google Search: ")];
            for (i, (label, url)) in chips.iter().enumerate() {
                if i > 0 {
                    line.push(RText::plain(" · "));
                }
                line.push(link(label, url));
            }
            out.push(RBlock::Paragraph(RText::Concat(line)));
        } else {
            out.push(json_block(sg));
        }
    }
    // URL context (Gemini): each URL and how fetching it went.
    for k in ["url_metadata", "urlMetadata"] {
        if let Some(Value::Array(a)) = f.get(k) {
            let items = a
                .iter()
                .map(|m| {
                    let u = m.get("retrievedUrl").or_else(|| m.get("retrieved_url")).and_then(Value::as_str);
                    let st =
                        m.get("urlRetrievalStatus").or_else(|| m.get("url_retrieval_status")).and_then(Value::as_str);
                    let blocks = match (u, st, m.as_object().map(|x| x.len())) {
                        (Some(u), Some(st), Some(2)) => {
                            vec![RBlock::Paragraph(RText::Concat(vec![
                                link(u, u),
                                RText::plain(format!(" · {}", url_status(st))),
                            ]))]
                        }
                        _ => vec![json_block(m)],
                    };
                    RItem { checkbox: None, blocks }
                })
                .collect();
            out.push(RBlock::List { items });
        }
    }
    // A fetched page (Claude web_fetch): the page, its type and when it was retrieved.
    if o.contains_key("url") && !o.contains_key("sources") {
        let url = f.str("url").unwrap_or_default().to_string();
        // The page as a document (`content: {type: document, title, source}`): its
        // body is not kept (a note of its length is).
        let doc = o.get("content").filter(|c| c.get("type").and_then(Value::as_str) == Some("document"));
        let mut note = None;
        let mut doc_title = None;
        let mut doc_type = None;
        let mut doc_rest = None;
        if let Some(d) = doc {
            f.skip("content");
            doc_title = d.get("title").and_then(Value::as_str).map(str::to_string);
            doc_type = d.pointer("/source/media_type").and_then(Value::as_str).map(str::to_string);
            note = d.pointer("/source/data").and_then(Value::as_str).filter(|x| x.starts_with('(')).map(str::to_string);
            let mut extra = d.as_object().cloned().unwrap_or_default();
            for k in ["type", "title", "source"] {
                extra.remove(k);
            }
            if let Some(src) = d.get("source").and_then(Value::as_object) {
                let mut src = src.clone();
                for k in ["type", "media_type", "data"] {
                    src.remove(k);
                }
                if !src.is_empty() {
                    extra.insert("source".into(), Value::Object(src));
                }
            }
            if !extra.is_empty() {
                doc_rest = Some(Value::Object(extra));
            }
        }
        let title = f.str("title").map(str::to_string).or(doc_title).unwrap_or_default();
        let mut line = vec![link(&title, &url)];
        let mut about = Vec::new();
        if let Some(m) = f.str("media_type").map(str::to_string).or(doc_type) {
            about.push(m);
        }
        if let Some(r) = f.str("retrieved_at") {
            about.push(format!("retrieved {r}"));
        }
        if !about.is_empty() {
            line.push(RText::plain(format!(" · {}", about.join(" · "))));
        }
        if o.get("type").and_then(Value::as_str).is_some_and(|t| t.ends_with("_result")) {
            f.skip("type");
        }
        out.push(RBlock::Paragraph(RText::Concat(line)));
        if let Some(n) = note {
            out.push(RBlock::Footer(RText::plain(n)));
        }
        if let Some(r) = doc_rest {
            out.push(rest_block(&r));
        }
    }
    // Claude's text editor: what happened to the file, what it read or changed.
    if let Some(b) = f.get("is_file_update").and_then(Value::as_bool) {
        out.push(text(if b { "updated the file" } else { "created the file" }));
    }
    if let (Some(start), Some(n)) =
        (f.get("start_line").and_then(Value::as_u64), f.get("num_lines").and_then(Value::as_u64))
    {
        let total = f.get("total_lines").and_then(Value::as_u64).map(|t| format!(" of {t}")).unwrap_or_default();
        out.push(text(&format!("lines {start}–{}{total}", start + n.saturating_sub(1))));
    }
    if let Some(c) = o.get("content").and_then(Value::as_str) {
        f.skip("content");
        f.skip("file_type");
        if !c.trim().is_empty() {
            out.push(code(c, ""));
        }
    }
    if let Some(Value::Array(lines)) = f.get("lines")
        && lines.iter().all(Value::is_string)
    {
        let diff: Vec<&str> = lines.iter().filter_map(Value::as_str).collect();
        out.push(code(&diff.join("\n"), "diff"));
        for k in ["old_start", "old_lines", "new_start", "new_lines"] {
            f.skip(k);
        }
    }
    // What an image generation used (OpenAI/xAI): size, quality, format…
    if t.name.ends_with("_generation") {
        let mut used = Vec::new();
        for k in ["action", "size", "quality", "background", "output_format"] {
            if let Some(v) = f.str(k) {
                used.push(v.to_string());
            }
        }
        if !used.is_empty() {
            out.push(text(&used.join(" · ")));
        }
    }
    if out.is_empty() {
        // Nothing recognised: the fields as they are (flat ones on a line, else the JSON).
        return vec![rest_block(&Value::Object(o.clone()))];
    }
    // The result envelope's type repeats the tool's name.
    if o.get("type").and_then(Value::as_str).is_some_and(|t| t.ends_with("_result")) {
        f.skip("type");
    }
    if let Some(rest) = f.rest() {
        out.push(rest_block(&rest));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(name: &str, input: Value, output: Value) -> ToolCall {
        ToolCall {
            id: Some("call_1".into()),
            name: name.into(),
            status: Some("completed".into()),
            input: Some(input).filter(|v| !v.is_null()),
            output: Some(output).filter(|v| !v.is_null()),
        }
    }

    /// The blocks as plain text, JSON blocks marked, to compare shapes.
    fn shown(blocks: &[RBlock]) -> String {
        let mut s = String::new();
        for b in blocks {
            match b {
                RBlock::Paragraph(t) | RBlock::Footer(t) => s.push_str(&format!("{}\n", t.to_plain())),
                RBlock::Pre { text, language } if language == "json" => s.push_str(&format!("JSON {text}\n")),
                RBlock::Pre { text, language } => s.push_str(&format!("```{language}\n{text}\n```\n")),
                RBlock::Quote { blocks } => s.push_str(&format!("> {}", shown(blocks))),
                RBlock::List { items } | RBlock::OrderedList { items, .. } => {
                    for i in items {
                        s.push_str(&format!("- {}", shown(&i.blocks)));
                    }
                }
                other => s.push_str(&format!("{other:?}\n")),
            }
        }
        s
    }

    #[test]
    fn searches_and_their_sources() {
        // OpenAI: queries in the action, sources as `{sources: [...]}`.
        let t = call(
            "web_search",
            json!({"type": "search", "queries": ["rust latest", "rust 1.100 date"], "query": "rust latest"}),
            json!({"sources": [{"type": "url", "url": "https://blog.rust-lang.org/a"}, {"type": "url", "url": "https://x.org/b", "title": "B"}]}),
        );
        assert_eq!(shown(&blocks(&t)), "🔎 rust latest\n🔎 rust 1.100 date\n- https://blog.rust-lang.org/a\n- B\n");
        // Claude: results with the page age; the opaque blob is left out.
        let t = call(
            "web_search",
            json!({"query": "rust"}),
            json!([{"type": "web_search_result", "title": "Rust 1.100", "url": "https://p.com/r", "page_age": "0 days ago", "encrypted_content": "Eu4f…"}]),
        );
        assert_eq!(shown(&blocks(&t)), "🔎 rust\n- Rust 1.100 · 0 days ago\n");
        // Zhipu: reference ids, site and date, an excerpt of the content.
        let t = call(
            "web_search",
            Value::Null,
            json!([{"refer": "ref_1", "title": "Rust 1.85", "link": "https://v.com/1", "media": "VPS911", "publish_date": "2025-03-04", "content": "Rust 1.85 released …", "icon": "https://v.com/i.png"}]),
        );
        assert_eq!(shown(&blocks(&t)), "- [ref_1] Rust 1.85 · VPS911 · 2025-03-04\nRust 1.85 released …\n");
    }

    #[test]
    fn page_actions_and_fetches() {
        let open = call("web_search", json!({"type": "open_page", "url": "https://r.org/x"}), Value::Null);
        assert_eq!(shown(&blocks(&open)), "open https://r.org/x\n");
        let find = call(
            "web_search",
            json!({"type": "find_in_page", "pattern": "1.100", "url": "https://r.org/y"}),
            Value::Null,
        );
        assert_eq!(shown(&blocks(&find)), "find “1.100” in https://r.org/y\n");
        let fetch = call(
            "web_fetch",
            json!({"url": "https://www.rust-lang.org/"}),
            json!({"url": "https://www.rust-lang.org/", "title": "Rust Programming Language", "media_type": "text/plain", "retrieved_at": "2026-11-12T05:15:38+00:00"}),
        );
        assert_eq!(
            shown(&blocks(&fetch)),
            "https://www.rust-lang.org/\nRust Programming Language · text/plain · retrieved 2026-11-12T05:15:38+00:00\n"
        );
        let ctx = call(
            "url_context",
            json!({"urls": ["https://rust-lang.org/"]}),
            json!({"url_metadata": [{"retrievedUrl": "https://rust-lang.org/", "urlRetrievalStatus": "URL_RETRIEVAL_STATUS_PAYWALL"}]}),
        );
        assert_eq!(shown(&blocks(&ctx)), "- https://rust-lang.org/\n- https://rust-lang.org/ · paywall\n");
        assert_eq!(outcome(&ctx).as_deref(), Some("paywall"));
    }

    #[test]
    fn code_and_what_it_printed() {
        // Claude bash: the command, stdout, a failing exit code.
        let bash = call(
            "bash_code_execution",
            json!({"command": "python3 -c \"print(2**100)\""}),
            json!({"type": "bash_code_execution_result", "stdout": "1267\n", "stderr": "warn\n", "return_code": 2, "content": []}),
        );
        assert_eq!(
            shown(&blocks(&bash)),
            "```bash\npython3 -c \"print(2**100)\"\n```\n```\n1267\n```\nstderr:\n```\nwarn\n```\nexit 2\n"
        );
        assert_eq!(outcome(&bash).as_deref(), Some("exit 2"));
        // OpenAI: code as a string, logs; Alibaba's logs come fenced.
        let oa = call("code_interpreter", json!("2**100\n"), json!([{"type": "logs", "logs": "1267"}]));
        assert_eq!(shown(&blocks(&oa)), "```python\n2**100\n```\n```\n1267\n```\n");
        let ali = call("code_interpreter", json!("print(1)"), json!([{"type": "logs", "logs": "```\n1\n\n```"}]));
        assert_eq!(shown(&blocks(&ali)), "```python\nprint(1)\n```\n```\n1\n```\n");
        // xAI: the logs are a JSON string.
        let xai = call(
            "code_interpreter",
            json!("print(1)"),
            json!([{"type": "logs", "logs": "{\"stdout\":\"1\\n\",\"stderr\":\"\",\"exit_code\":0,\"command_timed_out\":true}"}]),
        );
        assert_eq!(shown(&blocks(&xai)), "```python\nprint(1)\n```\n```\n1\n```\ntimed out\n");
        assert_eq!(outcome(&xai).as_deref(), Some("timed out"));
        // Gemini: language, the call id (same as the call's) left out, the outcome.
        let g = call(
            "code_execution",
            json!({"language": "PYTHON", "code": "print(1)\n", "id": "call_1"}),
            json!({"outcome": "OUTCOME_DEADLINE_EXCEEDED", "output": "partial\n", "id": "call_1"}),
        );
        assert_eq!(shown(&blocks(&g)), "```python\nprint(1)\n```\n```\npartial\n```\noutcome: deadline_exceeded\n");
        assert_eq!(outcome(&g).as_deref(), Some("deadline exceeded"));
        // Claude's text editor: the action on a path, the file's content.
        let ed = call(
            "text_editor_code_execution",
            json!({"command": "create", "path": "/tmp/plot.py", "file_text": "import x\n"}),
            json!({"type": "text_editor_code_execution_result", "is_file_update": false}),
        );
        assert_eq!(shown(&blocks(&ed)), "create /tmp/plot.py\n```py\nimport x\n```\ncreated the file\n");
        let view = call(
            "text_editor_code_execution",
            json!({"command": "view", "path": "/tmp/plot.py"}),
            json!({"type": "text_editor_code_execution_result", "file_type": "text", "content": "import x\n", "start_line": 1, "num_lines": 1, "total_lines": 1}),
        );
        assert_eq!(shown(&blocks(&view)), "view /tmp/plot.py\nlines 1–1 of 1\n```\nimport x\n```\n");
    }

    #[test]
    fn claude_fetch_files_and_nested_calls() {
        let fetch = call(
            "web_fetch",
            json!({"url": "https://www.rust-lang.org/", "caller": {"type": "code_execution_20260120", "tool_id": "srvtoolu_1"}}),
            json!({"type": "web_fetch_result", "url": "https://www.rust-lang.org/", "retrieved_at": "2026-11-12T05:15:38+00:00",
                   "content": {"type": "document", "title": "Rust Programming Language", "citations": {"enabled": true},
                               "source": {"type": "text", "media_type": "text/plain", "data": "(12,345 characters, sent back to the model in later turns; not shown)"}}}),
        );
        assert_eq!(
            shown(&blocks(&fetch)),
            "https://www.rust-lang.org/\n↳ called from code_execution\nRust Programming Language · text/plain · retrieved 2026-11-12T05:15:38+00:00\n(12,345 characters, sent back to the model in later turns; not shown)\ncitations.enabled: true\n"
        );
        let plot = call(
            "bash_code_execution",
            json!({"command": "python3 plot.py"}),
            json!({"type": "bash_code_execution_result", "stdout": "", "stderr": "", "return_code": 0,
                   "content": [{"type": "bash_code_execution_output", "file_id": "file_1", "file": {"filename": "sin.png", "size_bytes": 20480}}]}),
        );
        assert_eq!(shown(&blocks(&plot)), "```bash\npython3 plot.py\n```\n📎 sin.png (20.0 KB)\n");
    }

    #[test]
    fn errors_and_unknown_shapes() {
        let e = call(
            "web_search",
            json!({"query": "q"}),
            json!({"type": "web_search_tool_result_error", "error_code": "max_uses_exceeded"}),
        );
        assert_eq!(shown(&blocks(&e)), "🔎 q\n⚠️ max_uses_exceeded\n");
        assert_eq!(outcome(&e).as_deref(), Some("error: max_uses_exceeded"));
        // Unknown: the JSON as it is, nothing lost.
        let u = call("x_search", json!({"handles": ["a"]}), json!({"posts": [{"id": 1}]}));
        // Flat unknown fields: one line.
        let flat = call(
            "video_generation",
            json!({"prompt": "a candle", "duration": 1, "resolution": "720p"}),
            json!({"respect_moderation": true, "model": "grok-imagine-video-1.5-lite"}),
        );
        assert_eq!(
            shown(&blocks(&flat)),
            "> a candle\nduration: 1 · resolution: 720p\nrespect_moderation: true · model: grok-imagine-video-1.5-lite\n"
        );
        let s = shown(&blocks(&u));
        assert!(s.starts_with("JSON {\n  \"handles\""), "{s}");
        assert!(s.contains("JSON {\n  \"posts\""), "{s}");
        // Known fields readable, the unknown one kept as JSON.
        let mixed = call("web_search", json!({"query": "q", "max_results": 5}), Value::Null);
        assert_eq!(shown(&blocks(&mixed)), "🔎 q\nmax_results: 5\n");
    }

    #[test]
    fn image_generation_and_suggestions() {
        let img = call(
            "image_generation",
            json!("A cat on a mat"),
            json!({"size": "1024x1024", "quality": "high", "output_format": "png", "action": "generate"}),
        );
        assert_eq!(shown(&blocks(&img)), "> A cat on a mat\ngenerate · 1024x1024 · high · png\n");
        let g = call(
            "google_search",
            json!({"queries": ["rust version"]}),
            json!({"search_suggestions": [{"text": "rust version", "url": "https://www.google.com/search?q=rust+version&client=app-vertex-grounding"}]}),
        );
        assert_eq!(shown(&blocks(&g)), "🔎 rust version\nGoogle Search: rust version\n");
        // Gemini's tool response: the widget HTML; its chips are the suggestions.
        let html = call(
            "google_search",
            json!({"queries": ["rust version"]}),
            json!({"search_suggestions": "<style>\n.chip {}\n</style>\n<div class=\"container\">\n<a class=\"chip\" href=\"https://www.google.com/search?q=rust+version&amp;client=app-vertex-grounding\">rust version</a>\n</div>"}),
        );
        assert_eq!(shown(&blocks(&html)), "🔎 rust version\nGoogle Search: rust version\n");
    }
}
