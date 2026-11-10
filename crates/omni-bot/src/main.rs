//! omni: multi-bot, multi-provider Telegram LLM bot (MTProto).

mod access;
mod app;
mod commands;
mod compose;
mod config;
mod conv;
mod handler;
mod intake;
mod jobs;
mod ledger;
mod logging;
mod media;
mod rawlog;
mod reply;
mod store;
mod tg;
mod view;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use grammers_tl_types as tl;
use omni_llm::openrouter::options::{FidelityConfig, default_first_party};
use omni_llm::openrouter::{OpenRouter, OpenRouterConfig};
use parking_lot::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::app::App;
use crate::config::Config;
use crate::store::Db;
use crate::store::blobs::BlobStore;
use crate::tg::bot::BotConn;
use crate::tg::gate::Gates;

#[derive(Parser)]
#[command(name = "omni", version, about = "Multi-bot, multi-provider Telegram LLM bot")]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, short, env = "OMNI_CONFIG", default_value = "config/omni.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the bots (default).
    Run,
    /// Validate the configuration and exit.
    CheckConfig,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    if let Some(Cmd::CheckConfig) = cli.cmd {
        println!(
            "config OK: {} bot(s), {} alias(es), {} preset(s)",
            cfg.bots.len(),
            cfg.aliases.len(),
            cfg.presets.len()
        );
        return Ok(());
    }
    let log_dir = cfg.log.file.then(|| cfg.general.data_dir.join("logs"));
    let _guard = logging::init(&cfg.log.level, &cfg.general.timezone, log_dir.as_deref())?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let res = rt.block_on(run(cfg));
    if let Err(e) = &res {
        error!(error = %format!("{e:#}"), "fatal");
    }
    rt.shutdown_timeout(Duration::from_secs(5));
    res
}

async fn run(cfg: Config) -> Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "starting omni");
    let db = Db::open(&cfg.general.data_dir.join("omni.sqlite3"))?;
    let blobs = BlobStore::new(cfg.general.data_dir.join("blobs"), db.clone())?;
    access::seed(&db, &cfg.access.allowed_chats, &cfg.access.allowed_users).await?;

    let orc = cfg.providers.openrouter.clone().context("[providers.openrouter] is required")?;
    let openrouter = OpenRouter::new(OpenRouterConfig {
        api_key: orc.api_key.clone(),
        base_url: orc.base_url.clone(),
        http_referer: orc.http_referer.clone(),
        x_title: orc.x_title.clone(),
        connect_timeout: Duration::from_secs(orc.connect_timeout_secs),
        idle_timeout: Duration::from_secs(orc.idle_timeout_secs),
        fidelity: FidelityConfig {
            first_party: {
                let mut fp = default_first_party();
                for (k, v) in &orc.first_party {
                    fp.retain(|(a, _)| a != k);
                    fp.push((k.clone(), v.clone()));
                }
                fp
            },
            pin_first_party: orc.pin_first_party,
            require_parameters: orc.require_parameters,
            disable_context_compression: true,
            max_tokens_cap: (orc.max_tokens_cap > 0).then_some(orc.max_tokens_cap),
            long_decode: orc.long_decode.clone(),
            long_decode_budget: orc.long_decode_budget,
        },
        echo_upstream_body: orc.echo_upstream_body,
        router_metadata: true,
    })?;
    match openrouter.refresh_catalog().await {
        Ok(n) => info!(models = n, "loaded OpenRouter model catalog"),
        Err(e) => warn!(error = %format!("{e:#}"), "could not load the OpenRouter model catalog (will retry)"),
    }

    let api_id = cfg.telegram.api_id.as_i32()?;
    let group_iv = Duration::from_secs_f64(cfg.render.min_edit_interval_secs);
    let private_iv = Duration::from_secs_f64(cfg.render.min_edit_interval_secs.min(1.0));
    let mut bots = Vec::new();
    let mut streams = Vec::new();
    let connects = cfg.bots.iter().map(|b| {
        let db = db.clone();
        let hash = cfg.telegram.api_hash.clone();
        let gates = Gates::new(group_iv, private_iv);
        async move { (b.name.clone(), BotConn::connect(&db, b, api_id, &hash, gates).await) }
    });
    for (name, r) in futures::future::join_all(connects).await {
        let (conn, stream) = r.with_context(|| format!("connect bot {name}"))?;
        bots.push(conn);
        streams.push(stream);
    }

    let (turn_done, _) = tokio::sync::broadcast::channel(1024);
    let app = Arc::new(App {
        cfg: cfg.clone(),
        db: db.clone(),
        blobs,
        openrouter,
        bots: bots.clone(),
        membership: Mutex::new(HashSet::new()),
        aliases: RwLock::new(BTreeMap::new()),
        running: Mutex::new(HashSet::new()),
        turn_done,
        shutdown: CancellationToken::new(),
        started_at: jiff::Timestamp::now(),
    });
    app.load_runtime_state().await?;

    for b in &bots {
        if let Err(e) = set_commands(b).await {
            warn!(bot = %b.name, error = %e, "setting bot commands failed");
        }
    }

    let mut loops = Vec::new();
    for (b, s) in bots.iter().cloned().zip(streams) {
        loops.push(tokio::spawn(intake::run(app.clone(), b, s)));
    }
    handler::recover(app.clone()).await?;
    jobs::recover(app.clone()).await?;
    ledger::spawn_settler(app.clone());

    // Prefetch endpoint limits of the configured models, so `max_tokens` budgeting never waits.
    {
        let app = app.clone();
        tokio::spawn(async move {
            let mut models: Vec<String> = app
                .aliases
                .read()
                .values()
                .filter_map(|s| omni_core::trigger::parse_spec(s).ok())
                .map(|t| t.model.strip_prefix("openrouter:").unwrap_or(&t.model).to_string())
                .filter(|m| m.contains('/'))
                .collect();
            models.sort();
            models.dedup();
            app.openrouter.warm_endpoints(&models).await;
            info!(models = models.len(), "prefetched endpoint limits");
        });
    }

    // Periodic catalog refresh.
    {
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(600)) => {}
                    _ = app.shutdown.cancelled() => break,
                }
                if let Err(e) = app.openrouter.refresh_catalog().await {
                    warn!(error = %format!("{e:#}"), "catalog refresh failed");
                }
            }
        });
    }

    info!(bots = bots.len(), "omni is running");
    shutdown_signal().await;
    info!("shutting down: in-flight generations will resume after restart");
    app.shutdown.cancel();
    for l in loops {
        let _ = tokio::time::timeout(Duration::from_secs(5), l).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    for b in &bots {
        b.quit();
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn set_commands(bot: &BotConn) -> Result<()> {
    let cmds = [
        ("help", "How to use this bot"),
        ("models", "List model aliases / search models"),
        ("model", "Show a model's capabilities"),
        ("m", "Reply: continue with another model (spec$ text)"),
        ("raw", "Reply: get the original Markdown and files"),
        ("info", "Reply: show request details"),
        ("retry", "Reply: retry a failed answer"),
        ("stats", "Usage statistics"),
        ("usage", "Your spend limits"),
        ("whoami", "Show your user id"),
        ("ping", "Check the bot"),
    ];
    bot.invoke(&tl::functions::bots::SetBotCommands {
        scope: tl::enums::BotCommandScope::Default,
        lang_code: String::new(),
        commands: cmds
            .iter()
            .map(|(c, d)| {
                tl::types::BotCommand { ephemeral: false, command: c.to_string(), description: d.to_string() }.into()
            })
            .collect(),
    })
    .await?;
    Ok(())
}
