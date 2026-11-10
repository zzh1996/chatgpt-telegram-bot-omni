//! Logging setup.
//!
//! - Every log line carries the span context (`conv{bot=.. chat=.. user=.. turn=..}`),
//!   so a single conversation can be followed with `grep`.
//! - `verbose` (default) additionally logs every raw upstream request/response
//!   chunk under the `raw` target at TRACE level, with microsecond timestamps
//!   (useful to measure inter-token latency). Binary payloads are replaced by
//!   blob references (the bytes are in the blob store), so nothing is lost but
//!   logs stay readable.
//! - `normal` logs the request lifecycle at INFO.

use std::path::Path;

use anyhow::Result;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

struct Timer {
    tz: jiff::tz::TimeZone,
}

impl FormatTime for Timer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let now = jiff::Timestamp::now().to_zoned(self.tz.clone());
        write!(w, "{}", now.strftime("%Y-%m-%dT%H:%M:%S%.6f%:z"))
    }
}

pub fn filter_for(level: &str) -> String {
    match level {
        "verbose" => "info,raw=trace,omni_bot=debug,omni_llm=debug".into(),
        "normal" => "info,raw=off".into(),
        "quiet" => "warn,raw=off".into(),
        other => other.into(),
    }
}

/// Guards that flush the background log writers on drop; keep them alive.
pub struct LogGuards(#[allow(dead_code)] Vec<tracing_appender::non_blocking::WorkerGuard>);

/// Initialize logging.
///
/// Both stdout and the log file are written by background threads so that the
/// streaming code never waits on I/O, and both are **lossless**
/// (`lossy(false)`): under extreme load the producer is slowed down instead of
/// silently dropping lines — verbose logs must contain every raw payload.
pub fn init(level: &str, timezone: &str, log_dir: Option<&Path>) -> Result<LogGuards> {
    let tz = jiff::tz::TimeZone::get(timezone).unwrap_or(jiff::tz::TimeZone::UTC);
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| filter_for(level));
    let mut guards = Vec::new();
    let (stdout, g) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(false)
        .buffered_lines_limit(1 << 16)
        .finish(std::io::stdout());
    guards.push(g);
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let stdout_layer =
        fmt::layer().with_ansi(ansi).with_timer(Timer { tz: tz.clone() }).with_target(true).with_writer(stdout);
    let registry = tracing_subscriber::registry().with(EnvFilter::try_new(&filter)?).with(stdout_layer);
    if let Some(dir) = log_dir {
        std::fs::create_dir_all(dir)?;
        let appender = tracing_appender::rolling::daily(dir, "omni.log");
        let (writer, g) = tracing_appender::non_blocking::NonBlockingBuilder::default()
            .lossy(false)
            .buffered_lines_limit(1 << 16)
            .finish(appender);
        guards.push(g);
        let file_layer = fmt::layer().with_ansi(false).with_timer(Timer { tz }).with_writer(writer);
        registry.with(file_layer).init();
    } else {
        registry.init();
    }
    Ok(LogGuards(guards))
}
