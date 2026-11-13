//! omni: multi-bot, multi-provider Telegram LLM bot (MTProto).
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

mod access;
mod app;
mod commands;
mod compose;
mod config;
mod conv;
mod devinject;
mod handler;
mod help;
mod intake;
mod jobs;
mod ledger;
mod logging;
mod media;
mod policy;
mod prefixes;
mod rawlog;
mod reply;
mod store;
mod tg;
mod toolview;
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
    let _guard = logging::init(&cfg.log.level, &cfg.general.timezone, log_dir.as_deref(), cfg.log.keep_days)?;
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
    let bot_names: Vec<String> = cfg.bots.iter().map(|b| b.name.clone()).collect();
    // The administrators' private chats are allowed like the configured chats:
    // once, so that they can remove them again.
    let seeded: Vec<i64> = cfg.access.allowed_chats.iter().chain(&cfg.access.admins).copied().collect();
    access::seed(&db, &bot_names, &seeded).await?;

    // Without a key (or the section) OpenRouter is off: its models are not offered.
    let orc = match cfg.providers.openrouter.clone() {
        Some(c) => c,
        None => toml::from_str("").context("OpenRouter defaults")?,
    };
    let openrouter_on = !orc.api_key.trim().is_empty();
    if !openrouter_on {
        info!("OpenRouter is off (no api_key)");
    }
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
            pin_first_party: true,
            require_parameters: orc.require_parameters,
            disable_context_compression: true,
            max_tokens_cap: (orc.max_tokens_cap > 0).then_some(orc.max_tokens_cap),
            long_decode: orc.long_decode.clone(),
            long_decode_budget: orc.long_decode_budget,
        },
        echo_upstream_body: orc.echo_upstream_body,
        router_metadata: true,
    })?;
    // The platforms' own APIs: each one that has a key.
    let mut direct = std::collections::BTreeMap::new();
    for info in omni_llm::provider::PROVIDERS {
        let Some(pc) = cfg.providers.direct(info.id).filter(|p| !p.api_key.trim().is_empty()) else { continue };
        // Ark reserves TPM quota by the output budget: capped unless configured otherwise.
        let default_cap = if info.id == "ark" { Some(65_536) } else { None };
        let dc = omni_llm::provider::DirectConfig {
            api_key: pc.api_key.trim().to_string(),
            base_url: pc
                .base_url
                .clone()
                .unwrap_or_else(|| info.base_url.to_string())
                .trim_end_matches('/')
                .to_string(),
            connect_timeout: Duration::from_secs(pc.connect_timeout_secs),
            idle_timeout: Duration::from_secs(pc.idle_timeout_secs.unwrap_or(info.idle_timeout_secs)),
            max_tokens_cap: pc.max_tokens_cap.or(default_cap).filter(|&v| v > 0),
            cny_per_usd: pc.cny_per_usd,
            long_decode_budget: pc.long_decode_budget,
        };
        let p = omni_llm::provider::build(info.id, dc).with_context(|| format!("provider {}", info.id))?;
        info!(provider = info.id, "{} provider enabled", info.name);
        direct.insert(info.id.to_string(), p);
    }
    if openrouter_on {
        match openrouter.refresh_catalog().await {
            Ok(n) => info!(models = n, "loaded OpenRouter model catalog"),
            Err(e) => warn!(error = %format!("{e:#}"), "could not load the OpenRouter model catalog (will retry)"),
        }
    }

    let api_id = cfg.telegram.api_id.as_i32()?;
    let mut bots = Vec::new();
    let mut streams = Vec::new();
    let connects = cfg.bots.iter().map(|b| {
        let db = db.clone();
        let hash = cfg.telegram.api_hash.clone();
        async move {
            let conn = match Gates::load(&db, &b.name).await {
                Ok(gates) => BotConn::connect(&db, b, api_id, &hash, gates).await,
                Err(e) => Err(e),
            };
            (b.name.clone(), conn)
        }
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
        direct,
        allowed_providers: {
            let mut m = policy::default_allowed_providers();
            m.extend(orc.allowed_providers.clone());
            m
        },
        bots: bots.clone(),
        membership: Mutex::new(HashSet::new()),
        aliases: RwLock::new(BTreeMap::new()),
        chat_prefixes: RwLock::new(Default::default()),
        running: Mutex::new(HashSet::new()),
        turn_done,
        shutdown: CancellationToken::new(),
        started_at: jiff::Timestamp::now(),
    });
    app.load_runtime_state().await?;

    for b in &bots {
        if let Err(e) = set_commands(&app, b).await {
            warn!(bot = %b.name, error = %format!("{e:#}"), "registering the bot commands failed");
        }
    }

    let mut loops = Vec::new();
    for (b, s) in bots.iter().cloned().zip(streams) {
        loops.push(tokio::spawn(intake::run(app.clone(), b, s)));
    }
    handler::recover(app.clone()).await?;
    jobs::recover(app.clone()).await?;
    ledger::spawn_settler(app.clone());
    if let Some(dir) = &cfg.dev.inject_dir {
        devinject::spawn(app.clone(), dir.clone());
    }

    // Prefetch endpoint limits of the configured models, so `max_tokens` budgeting never waits.
    if openrouter_on {
        let app = app.clone();
        tokio::spawn(async move {
            let specs: Vec<String> = app.aliases.read().values().flat_map(|m| m.values().cloned()).collect();
            let mut models: Vec<String> = specs
                .iter()
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
    if openrouter_on {
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

/// Command lists that earlier versions (or the old bot) may have left for one
/// language or chat type. Telegram shows the most specific list, so a stale
/// one hides ours: e.g. the old bot's `en` list for every English client.
const STALE_COMMAND_LISTS: &[(tl::enums::BotCommandScope, &str)] = &[
    (tl::enums::BotCommandScope::Default, "en"),
    (tl::enums::BotCommandScope::Users, ""),
    (tl::enums::BotCommandScope::Users, "en"),
    (tl::enums::BotCommandScope::Chats, ""),
    (tl::enums::BotCommandScope::Chats, "en"),
    (tl::enums::BotCommandScope::ChatAdmins, ""),
    (tl::enums::BotCommandScope::ChatAdmins, "en"),
];

/// Register the command menu at every start (as the old bot did): one list for
/// every chat and language, after removing more specific lists that would hide
/// it; administrators also see the admin commands in their private chats.
async fn set_commands(app: &App, bot: &BotConn) -> Result<()> {
    let marker = app.marker();
    let continue_with = format!("Reply: continue with another model (prefix{marker}text)");
    let public = [
        ("help", "How to use this bot"),
        ("help_zh", "使用帮助（中文）"),
        ("models", "List model prefixes / search models"),
        ("model", "Show which model a prefix calls"),
        ("m", continue_with.as_str()),
        ("raw", "Reply: get the original Markdown and files"),
        ("full", "Reply: the answer's complete reasoning and tool calls"),
        ("dump", "Reply: the whole conversation as a JSON file"),
        ("prefix", "This chat's own prefixes: list, show, create"),
        ("unprefix", "Delete a prefix of this chat (its creator or an admin)"),
        ("info", "Reply: show request details"),
        ("retry", "Reply: retry a failed answer"),
        ("continue", "Reply: keep writing an answer that hit its long-decode budget"),
        ("stats", "Usage statistics"),
        ("usage", "Spend limits here: the chat's and yours"),
        ("ping", "Check the bot: your and this chat's ids, access here"),
    ];
    let admin = [
        ("allow", "Admin: allow a chat (group or private chat)"),
        ("deny", "Admin: remove a chat from the allow list"),
        ("ban", "Admin: ban someone in a group"),
        ("unban", "Admin: lift a ban in a group"),
        ("acl", "Admin: allowed chats and bans"),
        ("quota", "Admin: spend limits of a chat and its members"),
        ("alias", "Admin: add a model prefix"),
        ("unalias", "Admin: remove a model prefix"),
        ("credits", "Admin: OpenRouter key spending and platform balances"),
    ];
    let list = |cmds: &[(&str, &str)]| -> Vec<tl::enums::BotCommand> {
        cmds.iter()
            .map(|(c, d)| {
                tl::types::BotCommand { ephemeral: false, command: c.to_string(), description: d.to_string() }.into()
            })
            .collect()
    };
    for (scope, lang) in STALE_COMMAND_LISTS {
        bot.invoke(&tl::functions::bots::ResetBotCommands { scope: scope.clone(), lang_code: lang.to_string() })
            .await
            .with_context(|| format!("reset the {lang:?} command list"))?;
    }
    bot.invoke(&tl::functions::bots::SetBotCommands {
        scope: tl::enums::BotCommandScope::Default,
        lang_code: String::new(),
        commands: list(&public),
    })
    .await
    .context("set the command list")?;
    let all: Vec<_> = public.iter().chain(admin.iter()).copied().collect();
    for &id in &app.cfg.access.admins {
        // Only possible once the administrator has talked to this bot.
        let Ok(peer) = bot.input_peer(id).await else { continue };
        bot.invoke(&tl::functions::bots::SetBotCommands {
            scope: tl::types::BotCommandScopePeer { peer }.into(),
            lang_code: String::new(),
            commands: list(&all),
        })
        .await
        .with_context(|| format!("set the admin command list for {id}"))?;
    }
    info!(bot = %bot.name, "registered the bot commands");
    Ok(())
}
