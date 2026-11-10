//! Database schema and migrations.
//!
//! Migrations are append-only: never edit a released migration, add a new one.

pub const MIGRATIONS: &[&str] = &[
    // ---- v1
    r#"
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- grammers session storage, one logical session per bot
CREATE TABLE tg_session (
    bot TEXT PRIMARY KEY,
    home_dc INTEGER NOT NULL
);
CREATE TABLE tg_dc (
    bot TEXT NOT NULL,
    dc_id INTEGER NOT NULL,
    option TEXT NOT NULL,              -- JSON DcOption (includes auth key)
    PRIMARY KEY (bot, dc_id)
);
CREATE TABLE tg_peer (
    bot TEXT NOT NULL,
    peer_id INTEGER NOT NULL,          -- grammers PeerId (raw)
    info TEXT NOT NULL,                -- JSON PeerInfo
    PRIMARY KEY (bot, peer_id)
);
CREATE TABLE tg_update_state (
    bot TEXT PRIMARY KEY,
    pts INTEGER NOT NULL,
    qts INTEGER NOT NULL,
    date INTEGER NOT NULL,
    seq INTEGER NOT NULL
);
CREATE TABLE tg_channel_state (
    bot TEXT NOT NULL,
    channel_id INTEGER NOT NULL,
    pts INTEGER NOT NULL,
    PRIMARY KEY (bot, channel_id)
);

-- people and places (chat ids use the Bot API "marked" convention)
CREATE TABLE users (
    user_id INTEGER PRIMARY KEY,
    username TEXT,
    first_name TEXT,
    last_name TEXT,
    is_bot INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);
CREATE TABLE chats (
    chat_id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,                -- private | group | supergroup | channel
    title TEXT,
    username TEXT,
    updated_at INTEGER NOT NULL
);
CREATE TABLE bot_chats (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    status TEXT NOT NULL,              -- member | left
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (bot, chat_id)
);
CREATE TABLE acl (
    scope TEXT NOT NULL CHECK (scope IN ('user', 'chat')),
    id INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('allow', 'deny')),
    note TEXT,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);

-- every Telegram message the bot decided to act on (idempotent intake)
CREATE TABLE inbound (
    id INTEGER PRIMARY KEY,
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    msg_id INTEGER NOT NULL,
    sender_id INTEGER,
    date INTEGER NOT NULL,
    text TEXT NOT NULL DEFAULT '',
    reply_to_msg_id INTEGER,
    grouped_id INTEGER,
    payload TEXT NOT NULL DEFAULT '{}', -- JSON: entities, media descriptors, quote, topic...
    status TEXT NOT NULL DEFAULT 'new', -- new | done | ignored | error
    error TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (bot, chat_id, msg_id)
);
CREATE INDEX inbound_status ON inbound(status);
CREATE INDEX inbound_group ON inbound(bot, chat_id, grouped_id);

-- conversation tree
CREATE TABLE turns (
    id INTEGER PRIMARY KEY,
    parent_id INTEGER REFERENCES turns(id),
    role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
    data TEXT NOT NULL,                -- JSON omni_core::Turn
    spec TEXT,                         -- assistant: JSON resolved request spec
    status TEXT NOT NULL DEFAULT 'done', -- pending | running | done | failed | cancelled
    bot TEXT,
    chat_id INTEGER,
    user_id INTEGER,                   -- requesting user (for user turns: author)
    inbound_id INTEGER REFERENCES inbound(id),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX turns_parent ON turns(parent_id);
CREATE INDEX turns_status ON turns(status);

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY,
    turn_id INTEGER NOT NULL REFERENCES turns(id),
    n INTEGER NOT NULL,
    status TEXT NOT NULL,              -- running | done | failed | interrupted | cancelled
    request TEXT,                      -- JSON request body with blobs redacted
    output TEXT,                       -- JSON omni_core::Turn (partial or final)
    error TEXT,                        -- JSON error, verbatim upstream details
    upstream_id TEXT,                  -- generation / response id
    usage TEXT,                        -- JSON omni_core::Usage
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    UNIQUE (turn_id, n)
);

-- Telegram messages that belong to turns (any part of any attempt)
CREATE TABLE tg_messages (
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    msg_id INTEGER NOT NULL,
    turn_id INTEGER NOT NULL REFERENCES turns(id),
    attempt_id INTEGER REFERENCES attempts(id),
    part_index INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (bot, chat_id, msg_id)
);
CREATE INDEX tg_messages_turn ON tg_messages(turn_id);

-- outgoing messages, for idempotent send (random_id) and edit tracking
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY,
    bot TEXT NOT NULL,
    chat_id INTEGER NOT NULL,
    turn_id INTEGER,
    attempt_id INTEGER,
    part_index INTEGER NOT NULL DEFAULT 0,
    random_id INTEGER NOT NULL,
    reply_to INTEGER,
    msg_id INTEGER,                    -- NULL until the send is confirmed
    state TEXT NOT NULL,               -- pending | sent | deleted
    content_hash TEXT,                 -- hash of the last render applied to Telegram
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE (bot, random_id)
);
CREATE INDEX outbox_attempt ON outbox(attempt_id);

-- usage & cost ledger (one row per upstream call)
CREATE TABLE usage_ledger (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    bot TEXT,
    chat_id INTEGER,
    user_id INTEGER,
    turn_id INTEGER,
    attempt_id INTEGER UNIQUE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_tokens INTEGER,
    cached_tokens INTEGER,
    cache_write_tokens INTEGER,
    cost_usd REAL,
    cost_source TEXT,
    raw TEXT
);
CREATE INDEX usage_ts ON usage_ledger(ts);
CREATE INDEX usage_user ON usage_ledger(user_id, ts);
CREATE INDEX usage_chat ON usage_ledger(chat_id, ts);

-- token-bucket limits on spend (USD)
CREATE TABLE quota_rules (
    scope TEXT NOT NULL CHECK (scope IN ('user', 'chat')),
    id INTEGER NOT NULL,               -- 0 = default for the scope
    capacity_usd REAL NOT NULL,
    refill_usd_per_day REAL NOT NULL,
    set_by INTEGER,
    set_at INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);
CREATE TABLE quota_buckets (
    scope TEXT NOT NULL,
    id INTEGER NOT NULL,
    level_usd REAL NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (scope, id)
);

-- content-addressed blobs and Telegram file dedupe
CREATE TABLE blobs (
    sha256 TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mime TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE tg_files (
    kind TEXT NOT NULL,                -- photo | document
    file_id INTEGER NOT NULL,          -- Telegram photo/document id (global)
    sha256 TEXT NOT NULL,
    size INTEGER NOT NULL,
    mime TEXT,
    PRIMARY KEY (kind, file_id)
);

-- runtime settings and admin-defined aliases
CREATE TABLE settings (
    scope TEXT NOT NULL,               -- global | chat | user
    id INTEGER NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,               -- JSON
    PRIMARY KEY (scope, id, key)
);
CREATE TABLE aliases (
    name TEXT PRIMARY KEY,
    spec TEXT NOT NULL,                -- e.g. `openrouter:openai/gpt-6+effort=max`
    note TEXT,
    created_by INTEGER,
    created_at INTEGER NOT NULL
);
"#,
    // ---- v2: resumable checkpoints (long decode) per attempt
    r#"
ALTER TABLE attempts ADD COLUMN checkpoint TEXT;   -- JSON: {item, parts, usage, hops, prompt_tokens}
"#,
    // ---- v3: one ledger row per billed upstream call (idempotent by generation id),
    //          and calls whose usage never arrived (looked up later)
    r#"
CREATE TABLE usage_ledger_v3 (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    bot TEXT,
    chat_id INTEGER,
    user_id INTEGER,
    turn_id INTEGER,
    attempt_id INTEGER,
    generation_id TEXT UNIQUE,         -- upstream generation / response id
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    reasoning_tokens INTEGER,
    cached_tokens INTEGER,
    cache_write_tokens INTEGER,
    cost_usd REAL,
    cost_source TEXT,                  -- reported (in the stream) | fetched (looked up afterwards) | unknown
    raw TEXT
);
INSERT INTO usage_ledger_v3(id, ts, bot, chat_id, user_id, turn_id, attempt_id, provider, model, input_tokens,
    output_tokens, reasoning_tokens, cached_tokens, cache_write_tokens, cost_usd, cost_source, raw)
  SELECT id, ts, bot, chat_id, user_id, turn_id, attempt_id, provider, model, input_tokens,
    output_tokens, reasoning_tokens, cached_tokens, cache_write_tokens, cost_usd, cost_source, raw FROM usage_ledger;
DROP TABLE usage_ledger;
ALTER TABLE usage_ledger_v3 RENAME TO usage_ledger;
CREATE INDEX usage_ts ON usage_ledger(ts);
CREATE INDEX usage_user ON usage_ledger(user_id, ts);
CREATE INDEX usage_chat ON usage_ledger(chat_id, ts);
CREATE INDEX usage_attempt ON usage_ledger(attempt_id);

CREATE TABLE pending_costs (
    generation_id TEXT PRIMARY KEY,
    attempt_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    tries INTEGER NOT NULL DEFAULT 0,
    next_at INTEGER NOT NULL
);
"#,
];
