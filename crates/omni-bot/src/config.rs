//! Configuration file (`config/omni.toml`).
//!
//! Secrets are referenced as `${ENV_VAR}` (or `${ENV_VAR:-default}`) and
//! interpolated from the environment before parsing, so the TOML file itself
//! can be committed or shared without secrets.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    pub telegram: TelegramConfig,
    pub bots: Vec<BotConfig>,
    #[serde(default)]
    pub access: Access,
    #[serde(default)]
    pub providers: Providers,
    /// Prefixes: name (may be empty: `$text`) -> model spec, e.g. `c` -> `openrouter:anthropic/claude-opus-5.5`.
    #[serde(default)]
    pub aliases: Vec<AliasConfig>,
    /// Default request parameters by model pattern (the "max everything" presets).
    #[serde(default)]
    pub presets: Vec<PresetConfig>,
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub dev: DevConfig,
}

/// Developer/testing aids (all off by default).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DevConfig {
    /// Watch this directory for `*.json` simulated user messages
    /// (`{"chat_id":..,"user_id":..,"text":"..","reply_to":null,"bot":"main"}`):
    /// the bot posts each one visibly as a "simulated message" and handles it
    /// exactly like a real one. For end-to-end tests without a user account.
    pub inject_dir: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct General {
    /// Directory for the database, blobs and logs.
    pub data_dir: PathBuf,
    /// IANA time zone used for displaying times and day boundaries in stats.
    pub timezone: String,
    /// What ends the model prefix of a message: `c$hello` with the default `$`.
    /// Any non-empty text without letters, digits, whitespace and
    /// ``,+{}"=/-.:_\@` `` works, e.g. `%`, `¥` or `>>`.
    pub trigger_marker: String,
    /// Max number of models in one `a,b,c$` trigger.
    pub max_parallel_specs: usize,
    /// Max size of a Telegram file the bot downloads and forwards to a model (MB).
    pub max_file_mb: u64,
}

impl Default for General {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            timezone: "UTC".into(),
            trigger_marker: omni_core::trigger::DEFAULT_MARKER.into(),
            max_parallel_specs: 20,
            max_file_mb: 50,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    pub api_id: StringOrInt,
    pub api_hash: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrInt {
    Int(i64),
    Str(String),
}

impl StringOrInt {
    pub fn as_i32(&self) -> Result<i32> {
        match self {
            StringOrInt::Int(i) => Ok(i32::try_from(*i)?),
            StringOrInt::Str(s) => s.trim().parse().with_context(|| format!("not an integer: {s:?}")),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    /// Internal name (used in logs and the database). Must be unique and stable.
    pub name: String,
    pub token: String,
    /// The models this bot has: glob patterns over canonical model ids
    /// (`provider:author/model`), e.g. `openrouter:openai/*`. A model no pattern
    /// matches does not exist for the bot (not listed, not searchable, an
    /// unknown prefix), and neither do models of providers that are not
    /// configured. Every bot is independent: each one answers the prefixes it has.
    #[serde(default = "default_models")]
    pub models: Vec<String>,
    /// Removed: the empty prefix is an ordinary alias (`name = ""`).
    #[serde(default)]
    default_model: Option<String>,
}

impl BotConfig {
    /// Whether this bot has the canonical model id (`openrouter:author/model`, `ark:model`).
    pub fn serves(&self, canonical: &str) -> bool {
        self.models.iter().any(|p| glob_match(p, canonical))
    }
}

fn default_models() -> Vec<String> {
    vec!["*".into()]
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Access {
    /// Telegram user ids of administrators.
    pub admins: Vec<i64>,
    /// Chats (group ids, or user ids for private chats) allowed for every bot,
    /// once, at the first start after being added; then managed with /allow, /deny.
    pub allowed_chats: Vec<i64>,
    /// Users who may ask for higher-priority (more expensive) service tiers
    /// (`+tier=priority`, fast modes). Nobody gets them by default.
    pub priority_users: Vec<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Providers {
    pub openrouter: Option<OpenRouterConfig>,
    /// Volcengine Ark (火山方舟, Doubao); disabled when `api_key` is empty.
    pub ark: Option<ArkConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArkConfig {
    #[serde(default)]
    pub api_key: String,
    /// Whether users may type the id of a model of this provider that no prefix
    /// names, as a prefix of its own (`author/model$`, `ark:model$`). Off: only
    /// the prefixes of `[[aliases]]` and `/alias` reach this provider.
    #[serde(default)]
    pub allow_unlisted_models: bool,
    #[serde(default = "default_ark_base")]
    pub base_url: String,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Ark summary-mode models can pause long between chunks (Ark suggests ≥1800 s).
    #[serde(default = "default_ark_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Cap for the automatic max_tokens (answer only). Ark reserves TPM quota
    /// by prompt + max_tokens, so very large values throttle concurrency.
    #[serde(default = "default_ark_max_tokens_cap")]
    pub max_tokens_cap: u64,
    /// CNY per USD for cost estimates (Ark prices are in CNY; responses carry no cost).
    #[serde(default = "default_cny_per_usd")]
    pub cny_per_usd: f64,
}

fn default_ark_base() -> String {
    omni_llm::ark::DEFAULT_BASE_URL.into()
}
fn default_ark_idle_timeout() -> u64 {
    1800
}
fn default_ark_max_tokens_cap() -> u64 {
    65_536
}
fn default_cny_per_usd() -> f64 {
    7.1
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenRouterConfig {
    pub api_key: String,
    /// Whether users may type the id of a model of this provider that no prefix
    /// names, as a prefix of its own (`author/model$`, `ark:model$`). Off: only
    /// the prefixes of `[[aliases]]` and `/alias` reach this provider.
    #[serde(default)]
    pub allow_unlisted_models: bool,
    #[serde(default = "default_openrouter_base")]
    pub base_url: String,
    /// Optional app attribution headers.
    #[serde(default)]
    pub http_referer: Option<String>,
    #[serde(default)]
    pub x_title: Option<String>,
    /// Connect timeout (seconds).
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// Max idle time between stream chunks (seconds) before the attempt is considered stalled.
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Send a stable, salted hash of the Telegram user id as the `user` field (abuse attribution).
    #[serde(default)]
    pub user_id_salt: Option<String>,
    /// Only route to endpoints supporting every sent parameter (no silent dropping).
    #[serde(default = "yes")]
    pub require_parameters: bool,
    /// Ask OpenRouter to echo the exact upstream request (logged; shown by /info).
    #[serde(default = "yes")]
    pub echo_upstream_body: bool,
    /// Extra/overriding author -> first-party provider slug mappings.
    #[serde(default)]
    pub first_party: BTreeMap<String, String>,
    /// Cap for the automatic max_tokens (0 = no cap).
    #[serde(default = "default_max_tokens_cap")]
    pub max_tokens_cap: u64,
    /// Official providers users may pick with `+p=<slug>`, per model author
    /// (base slugs; regions included). Overrides the built-in list per author.
    #[serde(default)]
    pub allowed_providers: BTreeMap<String, Vec<String>>,
    /// Models (glob patterns) that support Gemini-style long decode
    /// (continuation hops); `max_tokens` is then a total budget across hops.
    #[serde(default = "default_long_decode")]
    pub long_decode: Vec<String>,
    /// Total output budget for long-decode models (`/continue` extends a reply
    /// that hit it by the same amount).
    #[serde(default = "default_max_tokens_cap")]
    pub long_decode_budget: u64,
}

fn default_long_decode() -> Vec<String> {
    vec!["google/gemini-4*".into()]
}

fn default_max_tokens_cap() -> u64 {
    393_216
}

fn yes() -> bool {
    true
}

fn default_openrouter_base() -> String {
    "https://openrouter.ai/api/v1".into()
}
fn default_connect_timeout() -> u64 {
    15
}
fn default_idle_timeout() -> u64 {
    // Long hidden thinking is fine: OpenRouter's keep-alive comments count as data.
    // This only catches connections that went silent altogether.
    3600
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasConfig {
    pub name: String,
    /// A spec in trigger syntax without the marker, e.g. `openrouter:anthropic/claude-opus-5.5+s`.
    pub spec: String,
    /// Free-form documentation in the config file (not used by the bot).
    #[serde(default)]
    #[allow(dead_code)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    /// Glob over canonical model ids, e.g. `openrouter:anthropic/*`.
    #[serde(rename = "match")]
    pub pattern: String,
    /// Default options for matching models, in the trigger option syntax
    /// (e.g. `"4"` for 4K images); the user's options override them. They are
    /// validated like the user's (no free-form request fields).
    pub options: String,
    /// Free-form documentation in the config file (not used by the bot).
    #[serde(default)]
    #[allow(dead_code)]
    pub note: Option<String>,
}

impl PresetConfig {
    /// The preset's options, parsed (`+` between options is optional).
    pub fn requested(&self) -> Result<omni_llm::request::Requested> {
        let spec = format!("x+{}", self.options.trim().trim_start_matches('+'));
        let t = omni_core::trigger::parse_spec(&spec).map_err(|e| anyhow::anyhow!("{e}"))?;
        omni_llm::request::parse(&t.options).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RenderConfig {
    /// `clean` (rendered Markdown) or `raw` (exact source with decorations).
    pub markdown: omni_markdown::Mode,
    /// Delay before the first flush of a new reply (seconds), to batch the first tokens.
    pub first_flush_delay_secs: f64,
    /// Keep the reasoning fold of a reply open while the model is still
    /// thinking. Off by default: every streaming edit sends the fold's state
    /// again, which would re-open it for anyone who closed it.
    pub reasoning_open: bool,
    /// Show usage/cost footer: `always`, `never`, or a USD threshold like `0.05`.
    pub show_usage: String,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            markdown: omni_markdown::Mode::Clean,
            first_flush_delay_secs: 1.0,
            reasoning_open: false,
            show_usage: "always".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RetryConfig {
    /// Max attempts per reply (including the first).
    pub max_attempts: u32,
    /// Wait before the 1st, 2nd, … retry (seconds); the last value repeats.
    /// A longer wait the upstream asks for (Retry-After, rate limits) wins.
    pub delays_secs: Vec<f64>,
    /// Settings of older versions (doubling backoff): accepted and ignored.
    #[allow(dead_code)]
    base_delay_secs: Option<f64>,
    #[allow(dead_code)]
    max_delay_secs: Option<f64>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self { max_attempts: 4, delays_secs: vec![2.0, 10.0, 30.0, 60.0], base_delay_secs: None, max_delay_secs: None }
    }
}

impl RetryConfig {
    /// Wait before retry number `n` (1-based).
    pub fn delay(&self, n: u32) -> std::time::Duration {
        let i = (n.max(1) - 1) as usize;
        let secs = self.delays_secs.get(i).or(self.delays_secs.last()).copied().unwrap_or(10.0);
        std::time::Duration::from_secs_f64(secs.max(0.0))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// `verbose` logs every raw upstream payload (blobs stored on disk, referenced by hash);
    /// `normal` logs request lifecycle only. Any tracing filter string is also accepted.
    pub level: String,
    /// Also write logs to `<data_dir>/logs/omni.YYYY-MM-DD.utc.log` (one file per UTC day).
    pub file: bool,
    /// Keep this many daily log files (0 = keep all, the default).
    pub keep_days: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self { level: "verbose".into(), file: true, keep_days: 0 }
    }
}

/// Replace `${VAR}` / `${VAR:-default}` with environment values.
pub fn interpolate_env(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").unwrap();
    let mut missing = Vec::new();
    let out = re.replace_all(raw, |c: &regex::Captures<'_>| {
        let name = &c[1];
        match lookup(name) {
            Some(v) => v,
            None => match c.get(2) {
                Some(d) => d.as_str().to_string(),
                None => {
                    missing.push(name.to_string());
                    String::new()
                }
            },
        }
    });
    if !missing.is_empty() {
        bail!("missing environment variables referenced by config: {}", missing.join(", "));
    }
    Ok(out.into_owned())
}

impl Config {
    pub fn general_max_file_bytes(&self) -> u64 {
        self.general.max_file_mb * 1_000_000
    }

    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        let cfg =
            Self::parse(&raw, &|k| std::env::var(k).ok()).with_context(|| format!("config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse TOML, then interpolate `${VAR}` in string values only (never in comments).
    pub fn parse(raw: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let mut v: toml::Value = toml::from_str(raw).context("TOML syntax")?;
        let mut missing = Vec::new();
        interpolate_value(&mut v, lookup, &mut missing);
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            bail!("missing environment variables referenced by config: {}", missing.join(", "));
        }
        v.try_into().context("config structure")
    }

    pub fn validate(&self) -> Result<()> {
        if self.bots.is_empty() {
            bail!("at least one [[bots]] entry is required");
        }
        let mut names = std::collections::HashSet::new();
        for b in &self.bots {
            if !names.insert(&b.name) {
                bail!("duplicate bot name {:?}", b.name);
            }
            if !b.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                bail!("bot name {:?} must be [A-Za-z0-9_-]", b.name);
            }
            if b.token.split(':').next().and_then(|s| s.parse::<i64>().ok()).is_none() {
                bail!("bot {:?}: token does not look like a bot token", b.name);
            }
            if let Some(d) = &b.default_model {
                bail!(
                    "bot {:?}: `default_model` was removed. The empty prefix is an ordinary alias now: \
                     [[aliases]] name = \"\" spec = \"<spec of {d:?}>\"",
                    b.name
                );
            }
        }
        let marker = &self.general.trigger_marker;
        omni_core::trigger::check_marker(marker).map_err(|e| anyhow::anyhow!("general.trigger_marker: {e}"))?;
        let mut aliases: BTreeMap<&str, ()> = BTreeMap::new();
        for a in &self.aliases {
            if aliases.insert(a.name.as_str(), ()).is_some() {
                bail!("duplicate alias {:?}", a.name);
            }
            if !omni_core::trigger::alias_name_ok(&a.name, marker) {
                bail!("alias {:?} contains the trigger marker {marker:?}, whitespace or one of `,+{{}}`", a.name);
            }
            omni_core::trigger::parse_spec(&a.spec).map_err(|e| anyhow::anyhow!("alias {:?}: spec {:?}: {e}", a.name, a.spec))?;
        }
        for p in &self.presets {
            p.requested().with_context(|| format!("preset {:?}", p.pattern))?;
        }
        self.telegram.api_id.as_i32().context("telegram.api_id")?;
        Ok(())
    }

    pub fn bot_id_of(token: &str) -> Option<i64> {
        token.split(':').next()?.parse().ok()
    }
}

fn interpolate_value(v: &mut toml::Value, lookup: &dyn Fn(&str) -> Option<String>, missing: &mut Vec<String>) {
    match v {
        toml::Value::String(s) => match interpolate_env(s, lookup) {
            Ok(n) => *s = n,
            Err(_) => {
                let re = regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}").unwrap();
                for c in re.captures_iter(s) {
                    if lookup(&c[1]).is_none() {
                        missing.push(c[1].to_string());
                    }
                }
            }
        },
        toml::Value::Array(a) => a.iter_mut().for_each(|x| interpolate_value(x, lookup, missing)),
        toml::Value::Table(t) => t.iter_mut().for_each(|(_, x)| interpolate_value(x, lookup, missing)),
        _ => {}
    }
}

/// Minimal glob: `*` matches any sequence, `?` one char. Case-sensitive.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    fn rec(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => rec(&p[1..], s) || (!s.is_empty() && rec(p, &s[1..])),
            (Some('?'), Some(_)) => rec(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => rec(&p[1..], &s[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = s.chars().collect();
    rec(&p, &s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delays() {
        let r = RetryConfig::default();
        let secs: Vec<u64> = (1..=6).map(|n| r.delay(n).as_secs()).collect();
        assert_eq!(secs, vec![2, 10, 30, 60, 60, 60]);
        let empty = RetryConfig { delays_secs: vec![], ..Default::default() };
        // A config written for an older version still loads.
        let old: RetryConfig = toml::from_str("max_attempts = 4\nbase_delay_secs = 10\nmax_delay_secs = 120").unwrap();
        assert_eq!(old.delay(1).as_secs(), 2);
        assert_eq!(empty.delay(1).as_secs(), 10);
    }

    #[test]
    fn env_interpolation() {
        let env = |k: &str| match k {
            "A" => Some("1".to_string()),
            _ => None,
        };
        assert_eq!(interpolate_env("x=${A} y=${B:-dflt}", &env).unwrap(), "x=1 y=dflt");
        assert!(interpolate_env("${MISSING}", &env).is_err());
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*", "openrouter:anthropic/claude"));
        assert!(glob_match("openrouter:anthropic/*", "openrouter:anthropic/claude-opus-5-5"));
        assert!(!glob_match("openrouter:anthropic/*", "openrouter:openai/gpt-6"));
        assert!(glob_match("*:google/gemini-?-*", "openrouter:google/gemini-4-carbon"));
    }

    #[test]
    fn a_bot_has_the_models_its_patterns_list() {
        let b = BotConfig {
            name: "b".into(),
            token: "1:a".into(),
            models: vec!["openrouter:openai/*".into(), "openrouter:anthropic/*".into()],
            default_model: None,
        };
        assert!(b.serves("openrouter:openai/gpt-chat-latest"));
        assert!(b.serves("openrouter:anthropic/claude-opus-5.5"));
        assert!(!b.serves("openrouter:bytedance-seed/seed-2.0-mini"));
        assert!(!b.serves("ark:doubao-seed-2-1-pro-260915"));
    }

    #[test]
    fn example_config_parses() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/omni.example.toml"))
            .expect("example config exists");
        let env = |k: &str| match k {
            "TELEGRAM_API_ID" => Some("12345".to_string()),
            "TELEGRAM_API_HASH" | "OPENROUTER_API_KEY" => Some("x".to_string()),
            "TELEGRAM_BOT_TOKEN" => Some("123:abc".to_string()),
            _ => None, // optional ones use ${VAR:-default}; commented-out ones must not be required
        };
        let cfg = Config::parse(&raw, &env).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.telegram.api_id.as_i32().unwrap(), 12345);
        assert!(cfg.aliases.iter().any(|a| a.name == "c"));
        let err = Config::parse(
            "[telegram]\napi_id = \"${NOPE}\"\napi_hash = \"x\"\n[[bots]]\nname=\"a\"\ntoken=\"1:a\"\n",
            &env,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("NOPE"));
    }
}
